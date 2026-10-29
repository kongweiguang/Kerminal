//! 片段目录浏览入口；编辑草稿通过单独弹层承载，目录保持紧凑可检索。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, Focusable as _, InteractiveElement as _, IntoElement, KeyDownEvent,
    StatefulInteractiveElement as _, Window,
    assets::IconName,
    component::{
        Disableable, Icon, Sizable as _,
        button::{Button, ButtonCustomVariant, ButtonVariants},
        checkbox::Checkbox,
        input::Input,
        tab::{Tab, TabBar},
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::commands::SnippetCatalogSection;
use kerminal_domain::commands::{
    SnippetCatalogItem, SnippetCatalogOrigin, SnippetPreferenceKey, SnippetScope,
};

use crate::theme::ThemeTokens;

use super::super::{
    CommandsView,
    snippet_browser::{
        is_snippet_search_shortcut, should_show_snippet_empty_state, snippet_page_is_stale,
        snippet_page_matches_filter,
    },
};

#[path = "snippet_dialog.rs"]
mod dialog;
pub(super) use dialog::compact_snippet_field;
#[path = "snippet_manager.rs"]
mod manager;
#[path = "snippet_preview.rs"]
mod preview;

/// 片段目录提示的标识、文案和可用动作必须来自同一渲染快照。
///
/// @author kongweiguang
struct SnippetCatalogNotice {
    id: &'static str,
    message: gpui_kit::SharedString,
    retryable: bool,
    is_error: bool,
    loading: bool,
}

impl CommandsView {
    /// 紧凑目录沿用 main 右栏的 12px 内边距和分组间距；宽工作区保持原排版。
    /// 搜索快捷键仅在片段子树内接管且不覆盖输入控件焦点；紧凑目录延续宿主表面，避免暗色下叠深第二层面板。
    /// 单行搜索尺寸通过 Styled 指定，避免 Input 的多行专用 h 忽略右栏规格。
    ///
    /// @author kongweiguang
    pub(super) fn render_snippets(
        &mut self,
        theme: ThemeTokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let toolbar = self.render_snippet_toolbar(theme, cx);
        let search_input = gpui_kit::Styled::h(
            Input::new(&self.snippet_query).w_full(),
            if self.compact_layout {
                px(36.)
            } else {
                px(32.)
            },
        );
        let search_input = if self.compact_layout {
            search_input
                .text_size(px(16.))
                .line_height(px(24.))
                .aria_label(crate::locale::text("搜索命令片段", "Search snippets"))
                .prefix(
                    Icon::new(IconName::Search)
                        .size(px(16.))
                        .text_color(theme.subtle),
                )
        } else {
            search_input
        };
        let search = div()
            .id("snippet-catalog-search")
            .w_full()
            .when(!self.compact_layout, |search| search.px_3().py_1())
            .child(search_input)
            .into_any_element();
        let section_tabs = self.render_snippet_sections(theme, cx);
        let fill_blocked = self.snippet_terminal_fill_blocked();
        let fill_feedback = if fill_blocked {
            Some(
                self.render_snippet_fill_feedback(theme, cx)
                    .into_any_element(),
            )
        } else {
            None
        };
        let loading = self.snippet_page_loading;
        let total = self.snippet_total_count;
        let query = self.snippet_query.read(cx).value().to_string();
        let loaded_section = self.snippet_page_loaded_section;
        let loaded_query = self.snippet_page_loaded_query.clone();
        let loaded_filter_matches = snippet_page_matches_filter(
            loaded_section,
            loaded_query.as_deref(),
            self.snippet_catalog_section,
            &query,
        );
        let has_error = self.snippet_page_error.is_some();
        let has_loaded_page = loaded_section.is_some();
        let rows_stale = snippet_page_is_stale(
            loaded_section,
            loaded_query.as_deref(),
            self.snippet_catalog_section,
            &query,
            has_error,
            self.snippet_page_retry_cursor.is_some(),
        );
        let row_items = self.snippet_page_items.clone();
        let rows = row_items
            .into_iter()
            .map(|item| self.render_snippet_row(item, rows_stale, theme, cx))
            .collect::<Vec<_>>();
        let empty_state_visible = should_show_snippet_empty_state(
            self.snippet_page_items.len(),
            loading,
            has_error,
            loaded_filter_matches,
        );
        let empty_state = if empty_state_visible {
            Some(self.render_snippet_empty_state(theme, !query.trim().is_empty(), cx))
        } else {
            None
        };
        let count_label = has_loaded_page.then(|| {
            if rows_stale {
                let filter = snippet_filter_description(
                    loaded_section.unwrap_or(self.snippet_catalog_section),
                    loaded_query.as_deref().unwrap_or_default(),
                );
                crate::locale::text(
                    format!("上次成功筛选：{filter} · 共 {total} 个片段"),
                    format!("Last successful filter: {filter} · {total} snippets"),
                )
            } else {
                crate::locale::text(format!("共 {total} 个片段"), format!("{total} snippets"))
            }
        });
        let notice = if let Some(error) = self.snippet_page_error.clone() {
            let detail = if !has_loaded_page {
                error
            } else if rows_stale {
                let filter = snippet_filter_description(
                    loaded_section.unwrap_or(self.snippet_catalog_section),
                    loaded_query.as_deref().unwrap_or_default(),
                );
                format!("{error} · 已保留上次成功的“{filter}”结果，旧条目暂不可操作。")
            } else {
                format!("{error} · 已加载的片段仍可用，可重试读取后续内容。")
            };
            Some(self.render_snippet_catalog_notice(
                SnippetCatalogNotice {
                    id: "snippet-catalog-error",
                    message: detail.into(),
                    retryable: true,
                    is_error: true,
                    loading,
                },
                theme,
                cx,
            ))
        } else if rows_stale && has_loaded_page {
            let filter = snippet_filter_description(
                loaded_section.unwrap_or(self.snippet_catalog_section),
                loaded_query.as_deref().unwrap_or_default(),
            );
            Some(self.render_snippet_catalog_notice(
                SnippetCatalogNotice {
                    id: "snippet-catalog-stale",
                    message: crate::locale::text(
                        format!("正在读取新筛选；当前仍显示上次成功的“{filter}”，旧条目暂不可操作。"),
                        format!("Loading the new filter; showing the last successful “{filter}” results. Actions are paused."),
                    ),
                    retryable: false,
                    is_error: false,
                    loading,
                },
                theme,
                cx,
            ))
        } else if !has_loaded_page && !loading {
            Some(self.render_snippet_catalog_notice(
                SnippetCatalogNotice {
                    id: "snippet-catalog-unloaded",
                    message: crate::locale::text(
                        "片段目录尚未读取。",
                        "The snippet catalog has not loaded yet.",
                    ),
                    retryable: true,
                    is_error: false,
                    loading,
                },
                theme,
                cx,
            ))
        } else {
            None
        };
        div()
            .id("command-snippets-view")
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if view.snippet_editor_open
                    || !is_snippet_search_shortcut(
                        &event.keystroke.key,
                        modifiers.control,
                        modifiers.platform,
                        modifiers.alt,
                        modifiers.shift,
                    )
                {
                    return;
                }
                let search_focused = view
                    .snippet_query
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window);
                let variable_editor_focused = view
                    .snippet_variables
                    .iter()
                    .any(|field| field.input.read(cx).focus_handle(cx).is_focused(window));
                if search_focused || variable_editor_focused {
                    return;
                }
                view.snippet_query
                    .update(cx, |input, cx| input.focus(window, cx));
                cx.stop_propagation();
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .min_h(px(0.))
            .when(self.compact_layout, |view| view.gap_3().px_3().pb_3())
            .overflow_hidden()
            .bg(if self.compact_layout {
                theme.tool_panel_surface
            } else {
                theme.materials.shell
            })
            .child(toolbar)
            .child(search)
            .child(section_tabs)
            .when_some(fill_feedback, |view, feedback| view.child(feedback))
            .when_some(notice, |view, notice| view.child(notice))
            .when_some(
                count_label.filter(|_| !self.compact_layout),
                |view, count| {
                    view.child(
                        div()
                            .id("snippet-catalog-count")
                            .w_full()
                            .px_3()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(count),
                    )
                },
            )
            .child(
                div()
                    .id("snippet-catalog-list")
                    .flex_1()
                    .min_h(px(0.))
                    .w_full()
                    .overflow_y_scroll()
                    .when(self.compact_layout, |list| {
                        list.border_y_1().border_color(theme.border)
                    })
                    .children(rows)
                    .when_some(empty_state, |list, empty_state| list.child(empty_state))
                    .when(loading, |list| {
                        list.child(
                            div()
                                .id("snippet-catalog-loading")
                                .w_full()
                                .p_3()
                                .text_color(theme.muted)
                                .child(crate::locale::text("正在加载片段…", "Loading snippets…")),
                        )
                    }),
            )
            .into_any_element()
    }

    /// 错误与旧筛选提示固定在列表前；重试重新走原查询用例，不复用过期行的动作。
    ///
    /// @author kongweiguang
    fn render_snippet_catalog_notice(
        &self,
        notice: SnippetCatalogNotice,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let SnippetCatalogNotice {
            id,
            message,
            retryable,
            is_error,
            loading,
        } = notice;
        div()
            .id(id)
            .w_full()
            .flex()
            .items_start()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.materials.panel)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_size(theme.text_xs)
                    .text_color(if is_error { theme.warning } else { theme.muted })
                    .child(message),
            )
            .when(retryable, |view| {
                view.child(
                    Button::new("snippet-catalog-retry")
                        .compact()
                        .secondary()
                        .label(crate::locale::text("重试", "Retry"))
                        .disabled(loading)
                        .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                            view.retry_snippet_page(window, cx);
                        })),
                )
            })
            .into_any_element()
    }

    /// Root 提供唯一模态表面；按 main 实窗的 17px 标题和紧邻两行候选排版，Unknown 只开放原批次查询。
    ///
    /// @author kongweiguang
    pub(super) fn render_snippet_import_review(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        use super::super::snippet_transfer_ui::SnippetImportPhase;

        let Some(review) = self.snippet_import_review.as_ref() else {
            return div().id("snippet-import-review-empty").into_any_element();
        };
        let phase = review.phase;
        let candidates_empty = review.candidates.is_empty();
        let candidate_count = review.candidates.len();
        let candidate_rows = review
            .candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let source_index = candidate.source_index;
                let selected = candidate.selected;
                let title = candidate.title.clone();
                let command = candidate.command.clone();
                let conflict = candidate.title_conflicts;
                let accessible_title =
                    crate::locale::text(format!("导入 {title}"), format!("Import {title}"));
                let candidate_entity = cx.entity().clone();
                Checkbox::new(format!("snippet-import-candidate-{source_index}"))
                    .checked(selected)
                    .disabled(!matches!(
                        phase,
                        SnippetImportPhase::Reviewing | SnippetImportPhase::NotCommitted
                    ))
                    .accessibility_label(accessible_title)
                    .w_full()
                    .gap_3()
                    .items_start()
                    .px_2()
                    .py_2()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_start()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .flex()
                                    .flex_col()
                                    .gap_0()
                                    .child(
                                        div()
                                            .w_full()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .text_size(px(12.))
                                            .line_height(px(16.))
                                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                                            .text_color(theme.foreground)
                                            .child(title),
                                    )
                                    .child(
                                        div()
                                            .w_full()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .font_family("monospace")
                                            .text_size(px(11.))
                                            .line_height(px(15.7143))
                                            .text_color(theme.muted)
                                            .child(command),
                                    ),
                            )
                            .when(conflict, |row| {
                                row.child(
                                    div()
                                        .flex_none()
                                        .whitespace_nowrap()
                                        .text_size(px(11.))
                                        .text_color(theme.warning)
                                        .child(crate::locale::text(
                                            "同名，将另存",
                                            "Same name; saved as a copy",
                                        )),
                                )
                            }),
                    )
                    .when(index + 1 < candidate_count, |row| {
                        row.border_b_1().border_color(theme.border)
                    })
                    .on_change(move |checked, _, app| {
                        candidate_entity.update(app, |view, cx| {
                            view.set_snippet_import_candidate_selected(source_index, *checked, cx);
                        });
                    })
            })
            .collect::<Vec<_>>();
        let status = review.status.clone();
        let issues = review
            .issues
            .iter()
            .enumerate()
            .map(|(index, issue)| {
                div()
                    .id(format!("snippet-import-issue-{index}"))
                    .w_full()
                    .text_size(px(12.))
                    .text_color(theme.error)
                    .child(issue.clone())
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let show_error_area = !issues.is_empty() || status.is_some();
        let can_change_selection = matches!(
            phase,
            SnippetImportPhase::Reviewing | SnippetImportPhase::NotCommitted
        );
        let can_close = !matches!(
            phase,
            SnippetImportPhase::Committing | SnippetImportPhase::OutcomeUnknown
        ) && !self.snippet_transfer_busy;
        let can_confirm =
            can_change_selection && review.selected_count() > 0 && !self.snippet_transfer_busy;
        let can_reconcile =
            phase == SnippetImportPhase::OutcomeUnknown && !self.snippet_transfer_busy;
        let status_color = match phase {
            SnippetImportPhase::OutcomeUnknown | SnippetImportPhase::NotCommitted => theme.warning,
            SnippetImportPhase::Committed => theme.success,
            SnippetImportPhase::Reviewing | SnippetImportPhase::Committing => theme.muted,
        };

        div()
            .id("snippet-import-review")
            .w_full()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .text_color(theme.foreground)
            .child(
                div()
                    .id("snippet-import-review-header")
                    .w_full()
                    .flex_shrink_0()
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
                            .child(crate::locale::text("导入命令片段", "Import snippets")),
                    )
                    .child(
                        Button::new("snippet-import-review-close")
                            .compact()
                            .focus_handle(&self.snippet_manager_focus)
                            .ghost()
                            .w(px(36.))
                            .h(px(36.))
                            .rounded(theme.control_radius)
                            .icon(IconName::X)
                            .accessibility_label(crate::locale::text("关闭", "Close"))
                            .tooltip(crate::locale::text("关闭", "Close"))
                            .disabled(!can_close)
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.cancel_snippet_import_review(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("snippet-import-review-body")
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .px_5()
                    .py_4()
                    .min_h(px(0.))
                    .when(show_error_area, |body| {
                        body.child(
                            div()
                                .id("snippet-import-review-errors")
                                .w_full()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .children(issues)
                                .when_some(status, |area, status| {
                                    area.child(
                                        div()
                                            .id("snippet-import-review-status")
                                            .w_full()
                                            .text_size(px(12.))
                                            .text_color(status_color)
                                            .child(status),
                                    )
                                }),
                        )
                    })
                    .when(candidates_empty, |body| {
                        body.child(
                            div()
                                .w_full()
                                .text_size(px(14.))
                                .text_color(theme.muted)
                                .child(crate::locale::text(
                                    "没有可导入的片段。",
                                    "No snippets to import.",
                                )),
                        )
                    })
                    .when(!candidates_empty, |body| {
                        body.child(
                            div()
                                .id("snippet-import-review-list")
                                .w_full()
                                .max_h(px(288.))
                                .overflow_y_scroll()
                                .border_y_1()
                                .border_color(theme.border)
                                .children(candidate_rows),
                        )
                    }),
            )
            .child(
                div()
                    .id("snippet-import-review-footer")
                    .w_full()
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .items_center()
                    .gap_2()
                    .px_5()
                    .py_4()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        snippet_import_button(
                            "snippet-import-review-cancel",
                            crate::locale::text("取消", "Cancel"),
                            None,
                            theme,
                        )
                        .ghost()
                        .disabled(!can_close)
                        .on_click(cx.listener(
                            |view, _: &ClickEvent, _, cx| {
                                view.cancel_snippet_import_review(cx);
                            },
                        )),
                    )
                    .when(phase == SnippetImportPhase::OutcomeUnknown, |footer| {
                        footer.child(
                            snippet_import_button(
                                "snippet-import-reconcile",
                                crate::locale::text("查询批次状态", "Check batch status"),
                                Some(IconName::RefreshCw),
                                theme,
                            )
                            .primary()
                            .disabled(!can_reconcile)
                            .on_click(cx.listener(
                                |view, _: &ClickEvent, _, cx| {
                                    view.reconcile_snippet_import(cx);
                                },
                            )),
                        )
                    })
                    .when(
                        matches!(
                            phase,
                            SnippetImportPhase::Reviewing
                                | SnippetImportPhase::NotCommitted
                                | SnippetImportPhase::Committing
                        ),
                        |footer| {
                            footer.child(
                                snippet_import_button(
                                    "snippet-import-confirm",
                                    crate::locale::text("导入所选", "Import selected"),
                                    Some(IconName::Upload),
                                    theme,
                                )
                                .primary()
                                .disabled(!can_confirm)
                                .on_click(cx.listener(
                                    |view, _: &ClickEvent, _, cx| {
                                        view.confirm_snippet_import(cx);
                                    },
                                )),
                            )
                        },
                    ),
            )
            .into_any_element()
    }

    /// 头部目标身份和状态分行，长主机名不会挤掉状态；操作仍只调用已接入的目录用例。
    ///
    /// @author kongweiguang
    fn render_snippet_toolbar(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let (target, target_status) = self.snippet_current_target_labels();
        let target_tooltip = target.clone();
        let transfer_available = self.snippet_transfer_files.is_some();
        let manager_available = self.configuration_directory.is_some();
        div()
            .id("snippet-catalog-toolbar")
            .w_full()
            .flex()
            .items_center()
            .gap_3()
            .when(self.compact_layout, |toolbar| toolbar.gap_2())
            .when(!self.compact_layout, |toolbar| toolbar.px_3().py_1())
            .child(
                div()
                    .id("snippet-current-target")
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .id("snippet-current-target-name")
                            .w_full()
                            .min_w(px(0.))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(theme.text_xs)
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .text_color(theme.foreground)
                            .aria_label(target.clone())
                            .tooltip(move |window, cx| {
                                Tooltip::new(target_tooltip.clone()).build(window, cx)
                            })
                            .child(target),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(11.))
                            .text_color(theme.muted)
                            .child(target_status),
                    ),
            )
            .child(
                Button::new("snippet-import")
                    .compact()
                    .ghost()
                    .w(if self.compact_layout {
                        px(36.)
                    } else {
                        px(32.)
                    })
                    .h(if self.compact_layout {
                        px(36.)
                    } else {
                        px(32.)
                    })
                    .icon(IconName::Upload)
                    .accessibility_label(crate::locale::text("导入片段", "Import snippets"))
                    .tooltip(crate::locale::text("导入片段", "Import snippets"))
                    .disabled(!transfer_available)
                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                        view.preview_snippet_import(cx);
                    })),
            )
            .child(
                Button::new("snippet-export")
                    .compact()
                    .ghost()
                    .w(if self.compact_layout {
                        px(36.)
                    } else {
                        px(32.)
                    })
                    .h(if self.compact_layout {
                        px(36.)
                    } else {
                        px(32.)
                    })
                    .icon(IconName::Download)
                    .accessibility_label(crate::locale::text("导出片段", "Export snippets"))
                    .tooltip(crate::locale::text("导出片段", "Export snippets"))
                    // 导出用例读取完整用户目录，不受当前目录筛选或分页结果影响。
                    .disabled(!transfer_available)
                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                        view.export_snippets(cx);
                    })),
            )
            .child(
                Button::new("snippet-library-manager")
                    .compact()
                    .ghost()
                    .w(if self.compact_layout {
                        px(36.)
                    } else {
                        px(32.)
                    })
                    .h(if self.compact_layout {
                        px(36.)
                    } else {
                        px(32.)
                    })
                    .icon(IconName::Settings2)
                    .accessibility_label(crate::locale::text(
                        "打开片段库管理",
                        "Open snippet library manager",
                    ))
                    .disabled(!manager_available)
                    .tooltip(crate::locale::text(
                        "打开片段库管理",
                        "Open snippet library manager",
                    ))
                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                        view.open_snippet_library_manager(cx);
                    })),
            )
            .child(
                Button::new("snippet-new")
                    .compact()
                    .primary()
                    .w(if self.compact_layout {
                        px(36.)
                    } else {
                        px(32.)
                    })
                    .h(if self.compact_layout {
                        px(36.)
                    } else {
                        px(32.)
                    })
                    .icon(IconName::Plus)
                    .accessibility_label(crate::locale::text("新建片段", "New snippet"))
                    .tooltip(crate::locale::text("新建片段", "New snippet"))
                    .disabled(self.snippet_saving || self.confirm_snippet_delete)
                    .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                        view.new_snippet(window, cx);
                    })),
            )
            .into_any_element()
    }

    /// 紧凑栏用等分 Tab 单元匹配 main 的无间距下划线；宽目录继续复用 Kit TabBar 的尺寸与滚动行为。
    /// 两种布局都由同一焦点句柄处理方向键和选择变化，避免切换布局后键盘路径分叉。
    ///
    /// @author kongweiguang
    fn render_snippet_sections(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let tabs = [
            (
                SnippetCatalogSection::Common,
                crate::locale::text("常用", "Common"),
            ),
            (
                SnippetCatalogSection::Mine,
                crate::locale::text("我的", "Mine"),
            ),
            (
                SnippetCatalogSection::Library,
                crate::locale::text("命令库", "Library"),
            ),
        ];
        let selected_index = section_index(self.snippet_catalog_section);
        let compact_layout = self.compact_layout;

        div()
            .id("snippet-catalog-section-focus")
            .w_full()
            .when(!self.compact_layout, |tabs| tabs.px_3().py_1())
            .track_focus(&self.snippet_catalog_focus)
            .tab_index(0)
            .tab_stop(true)
            .role(gpui_kit::Role::TabList)
            .aria_label(crate::locale::text("片段分类", "Snippet categories"))
            .aria_keyshortcuts("ArrowLeft ArrowRight Home End Enter Space")
            .focus_visible(|style| style.border_color(theme.accent))
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if modifiers.control || modifiers.alt || modifiers.platform || modifiers.shift {
                    return;
                }
                let current = section_index(view.snippet_catalog_section);
                let key = event.keystroke.key.to_ascii_lowercase();
                let target = match key.as_str() {
                    "left" | "arrowleft" => Some((current + 2) % 3),
                    "right" | "arrowright" => Some((current + 1) % 3),
                    "home" => Some(0),
                    "end" => Some(2),
                    "enter" | "space" | " " => Some(current),
                    _ => None,
                };
                if let Some(index) = target {
                    view.select_snippet_catalog_section(section_at(index), window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(if compact_layout {
                div()
                    .id("snippet-catalog-sections")
                    .w_full()
                    .h(px(32.))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.border)
                    .children(tabs.into_iter().map(|(section, label)| {
                        let selected = section == self.snippet_catalog_section;
                        let tab_label = label.clone();
                        div()
                            .id(format!("snippet-catalog-tab-{}", section_index(section)))
                            .flex_1()
                            .min_w(px(0.))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .border_b_2()
                            .border_color(if selected {
                                theme.accent
                            } else {
                                theme.tool_panel_surface
                            })
                            .text_size(px(16.))
                            .line_height(px(24.))
                            .font_weight(gpui_kit::FontWeight::NORMAL)
                            .text_color(if selected {
                                theme.foreground
                            } else {
                                theme.muted
                            })
                            .cursor_pointer()
                            .role(gpui_kit::Role::Tab)
                            .aria_label(tab_label)
                            .aria_selected(selected)
                            .aria_keyshortcuts("Enter Space")
                            .tab_index(-1)
                            .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                                window.focus(&view.snippet_catalog_focus, cx);
                                view.select_snippet_catalog_section(section, window, cx);
                            }))
                            .child(label)
                    }))
                    .into_any_element()
            } else {
                TabBar::new("snippet-catalog-sections")
                    .underline()
                    .small()
                    .selected_index(selected_index)
                    .w_full()
                    .h(px(32.))
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(theme.text_xs)
                    .children(tabs.into_iter().map(|(section, label)| {
                        let aria_label = label.clone();
                        Tab::new()
                            .flex_1()
                            .justify_center()
                            .aria_label(aria_label)
                            .label(label)
                            .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                                window.focus(&view.snippet_catalog_focus, cx);
                                view.select_snippet_catalog_section(section, window, cx);
                            }))
                    }))
                    .into_any_element()
            })
            .into_any_element()
    }

    /// 紧凑行复用 main 的双行摘要与整行切换命中区，宽目录保留较舒展的原布局。
    /// 预览仍单独滚动，避免窄栏内的长正文挤压行控件。
    /// 按钮悬停色通过 Kit variant 注入，避免重复注册组件内部的 hover refinement。
    ///
    /// @author kongweiguang
    fn render_snippet_row(
        &mut self,
        item: SnippetCatalogItem,
        stale: bool,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let key = SnippetPreferenceKey {
            origin: item.origin,
            snippet_id: item.id.clone(),
        };
        let title = item.title.clone();
        let description = item.description.clone();
        let favorite = item.favorite;
        let selected = self.selected_snippet_matches_key(&key);
        let preview_open = self.snippet_inline_preview.matches_key(&key);
        let (metadata, metadata_details) = row_metadata(
            item.origin,
            &item.category,
            item.scope,
            &item.platforms,
            &item.shells,
        );
        let open_key = key.clone();
        let chevron_key = key.clone();
        let favorite_key = key.clone();
        let preview = self.render_snippet_inline_preview(&key, stale, theme, cx);
        let preview_icon = if preview_open {
            IconName::ChevronUp
        } else {
            IconName::ChevronDown
        };
        let title_action = crate::locale::text(
            if preview_open {
                format!("{}，收起预览", title)
            } else {
                format!("{}，展开预览", title)
            },
            if preview_open {
                format!("{} · Collapse preview", title)
            } else {
                format!("{} · Expand preview", title)
            },
        );
        if self.compact_layout {
            let summary_is_template = description.trim().is_empty();
            let summary = if summary_is_template {
                item.template.clone()
            } else {
                description.clone()
            };
            let row_button_variant = ButtonCustomVariant::new(cx)
                .foreground(theme.foreground)
                .hover(theme.hover)
                .active(theme.hover);

            return div()
                .id(format!(
                    "snippet-row-{}-{}",
                    origin_key(item.origin),
                    item.id
                ))
                .w_full()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .border_b_1()
                .border_color(theme.border)
                .when(selected, |row| row.bg(theme.selected.opacity(0.22)))
                .child(
                    div()
                        .w_full()
                        .min_w(px(0.))
                        .flex()
                        .items_center()
                        .hover(|style| style.bg(theme.hover))
                        .child(
                            Button::new(format!(
                                "snippet-favorite-{}-{}",
                                origin_key(item.origin),
                                item.id
                            ))
                            .compact()
                            .custom(row_button_variant)
                            .ml_1()
                            .w(px(32.))
                            .h(px(32.))
                            .child(Icon::new(IconName::Star).size(px(14.)).text_color(
                                if favorite {
                                    theme.warning
                                } else {
                                    theme.subtle
                                },
                            ))
                            .accessibility_label(crate::locale::text(
                                if favorite {
                                    format!("取消收藏 {title}")
                                } else {
                                    format!("收藏 {title}")
                                },
                                if favorite {
                                    format!("Remove favorite {title}")
                                } else {
                                    format!("Favorite {title}")
                                },
                            ))
                            .disabled(stale)
                            .on_click(cx.listener(
                                move |view, _: &ClickEvent, _, cx| {
                                    view.set_snippet_favorite(favorite_key.clone(), !favorite, cx);
                                },
                            )),
                        )
                        .child(
                            Button::new(format!(
                                "snippet-open-{}-{}",
                                origin_key(item.origin),
                                item.id
                            ))
                            .compact()
                            .custom(row_button_variant)
                            .flex_1()
                            .min_w(px(0.))
                            .min_h(px(48.))
                            .px_2()
                            .py_2()
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .accessibility_label(title_action.clone())
                            .disabled(stale)
                            .text_color(if selected {
                                theme.accent
                            } else {
                                theme.foreground
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .w_full()
                                            .min_w(px(0.))
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .text_size(px(12.))
                                            .line_height(px(16.))
                                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                                            .text_color(if selected {
                                                theme.accent
                                            } else {
                                                theme.foreground
                                            })
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
                                            .line_height(px(16.5))
                                            .when(summary_is_template, |line| {
                                                line.font_family("monospace")
                                            })
                                            .text_color(theme.muted)
                                            .child(summary),
                                    ),
                            )
                            .child(
                                div()
                                    .id(format!(
                                        "snippet-metadata-{}-{}",
                                        origin_key(item.origin),
                                        item.id
                                    ))
                                    .flex_shrink_0()
                                    .text_size(px(10.))
                                    .line_height(px(15.))
                                    .text_color(theme.subtle)
                                    .whitespace_nowrap()
                                    .aria_label(metadata_details.clone())
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(metadata_details.clone()).build(window, cx)
                                    })
                                    .child(metadata),
                            )
                            .child(
                                Icon::new(preview_icon)
                                    .size(px(14.))
                                    .text_color(theme.subtle),
                            )
                            .on_click(cx.listener(
                                move |view, _: &ClickEvent, window, cx| {
                                    view.toggle_snippet_preview(open_key.clone(), window, cx);
                                },
                            )),
                        ),
                )
                .when_some(preview, |row, preview| row.child(preview))
                .into_any_element();
        }

        div()
            .id(format!(
                "snippet-row-{}-{}",
                origin_key(item.origin),
                item.id
            ))
            .w_full()
            .min_h(px(64.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .when(selected, |row| row.bg(theme.selected.opacity(0.22)))
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new(format!(
                            "snippet-favorite-{}-{}",
                            origin_key(item.origin),
                            item.id
                        ))
                        .compact()
                        .ghost()
                        .w(px(32.))
                        .h(px(32.))
                        .icon(IconName::Star)
                        .text_color(if favorite {
                            theme.warning
                        } else {
                            theme.subtle
                        })
                        .accessibility_label(crate::locale::text(
                            if favorite {
                                "取消收藏"
                            } else {
                                "收藏片段"
                            },
                            if favorite {
                                "Remove favorite"
                            } else {
                                "Favorite snippet"
                            },
                        ))
                        .disabled(stale)
                        .on_click(cx.listener(
                            move |view, _: &ClickEvent, _, cx| {
                                view.set_snippet_favorite(favorite_key.clone(), !favorite, cx);
                            },
                        )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap_0()
                            .child(
                                div()
                                    .w_full()
                                    .min_w(px(0.))
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Button::new(format!(
                                            "snippet-open-{}-{}",
                                            origin_key(item.origin),
                                            item.id
                                        ))
                                        .compact()
                                        .h(px(32.))
                                        .text_size(theme.text_body)
                                        .line_height(px(20.))
                                        .flex_1()
                                        .min_w(px(0.))
                                        .ghost()
                                        .justify_start()
                                        .accessibility_label(title_action.clone())
                                        .disabled(stale)
                                        .text_color(if selected {
                                            theme.accent
                                        } else {
                                            theme.foreground
                                        })
                                        .child(
                                            div()
                                                .w_full()
                                                .min_w(px(0.))
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .text_ellipsis()
                                                .child(title),
                                        )
                                        .on_click(
                                            cx.listener(move |view, _: &ClickEvent, window, cx| {
                                                view.toggle_snippet_preview(
                                                    open_key.clone(),
                                                    window,
                                                    cx,
                                                );
                                            }),
                                        ),
                                    )
                                    .child(
                                        div()
                                            .id(format!(
                                                "snippet-metadata-{}-{}",
                                                origin_key(item.origin),
                                                item.id
                                            ))
                                            .flex_shrink_0()
                                            .text_size(theme.text_xs)
                                            .line_height(px(20.))
                                            .text_color(theme.subtle)
                                            .whitespace_nowrap()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .aria_label(metadata_details.clone())
                                            .tooltip(move |window, cx| {
                                                Tooltip::new(metadata_details.clone())
                                                    .build(window, cx)
                                            })
                                            .child(metadata),
                                    ),
                            )
                            .when(!description.trim().is_empty(), |row| {
                                row.child(
                                    div()
                                        .w_full()
                                        .min_w(px(0.))
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_size(theme.text_xs)
                                        .line_height(px(20.))
                                        .text_color(theme.muted)
                                        .child(description),
                                )
                            }),
                    )
                    .child(
                        Button::new(format!(
                            "snippet-toggle-{}-{}",
                            origin_key(item.origin),
                            item.id
                        ))
                        .compact()
                        .ghost()
                        .w(px(32.))
                        .h(px(32.))
                        .icon(preview_icon)
                        .accessibility_label(title_action.clone())
                        .tooltip(title_action)
                        .disabled(stale)
                        .on_click(cx.listener(
                            move |view, _: &ClickEvent, window, cx| {
                                view.toggle_snippet_preview(chevron_key.clone(), window, cx);
                            },
                        )),
                    ),
            )
            .when_some(preview, |row, preview| row.child(preview))
            .into_any_element()
    }

    /// 空态按当前查询和分类提供可执行去向，避免展示不可操作的说明或伪造入口。
    ///
    /// @author kongweiguang
    fn render_snippet_empty_state(
        &mut self,
        theme: ThemeTokens,
        has_query: bool,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let section = self.snippet_catalog_section;
        let (message, action_label) = if has_query {
            (
                crate::locale::text("没有匹配项。", "No matching snippets."),
                crate::locale::text("清除搜索", "Clear search"),
            )
        } else {
            match section {
                SnippetCatalogSection::Common => (
                    crate::locale::text("常用片段会显示在这里。", "Common snippets appear here."),
                    crate::locale::text("浏览命令库", "Browse library"),
                ),
                SnippetCatalogSection::Mine => (
                    crate::locale::text("还没有自己的片段。", "You do not have any snippets yet."),
                    crate::locale::text("新建片段", "Create snippet"),
                ),
                SnippetCatalogSection::Library => (
                    crate::locale::text(
                        "命令库中没有匹配项。",
                        "No snippets in the library match.",
                    ),
                    crate::locale::text("返回常用", "Back to Common"),
                ),
            }
        };
        let action = if has_query {
            Button::new("snippet-empty-clear-query")
                .compact()
                .secondary()
                .label(action_label)
                .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                    view.set_snippet_query(String::new(), window, cx);
                }))
                .into_any_element()
        } else if section == SnippetCatalogSection::Mine {
            Button::new("snippet-empty-create")
                .compact()
                .secondary()
                .label(action_label)
                .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                    view.new_snippet(window, cx);
                }))
                .into_any_element()
        } else {
            let destination = if section == SnippetCatalogSection::Common {
                SnippetCatalogSection::Library
            } else {
                SnippetCatalogSection::Common
            };
            Button::new("snippet-empty-change-section")
                .compact()
                .secondary()
                .label(action_label)
                .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                    view.select_snippet_catalog_section(destination, window, cx);
                }))
                .into_any_element()
        };

        div()
            .id("snippet-catalog-empty")
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .p_4()
            .text_color(theme.muted)
            .child(message)
            .child(action)
            .into_any_element()
    }

    /// 目标名和状态分别投影，完整名称仍可由目标名 tooltip 检查。
    ///
    /// @author kongweiguang
    fn snippet_current_target_labels(&self) -> (gpui_kit::SharedString, gpui_kit::SharedString) {
        if let Some(label) = self.session_target_label() {
            return (
                crate::locale::text(label.to_owned(), label.to_owned()),
                crate::locale::text("当前填入目标", "Fill target"),
            );
        }
        let Some(info) = self.session_info.as_ref() else {
            return (
                crate::locale::text("未选择终端", "No terminal selected"),
                crate::locale::text("选择分屏后可填入命令", "Choose a pane to fill commands"),
            );
        };
        let (target_zh, target_en) = match &info.target {
            kerminal_domain::sessions::SessionTarget::Local(_) => ("本地终端", "Local terminal"),
            kerminal_domain::sessions::SessionTarget::Ssh(_) => ("SSH 终端", "SSH terminal"),
            kerminal_domain::sessions::SessionTarget::Telnet(_) => {
                ("Telnet 终端", "Telnet terminal")
            }
            kerminal_domain::sessions::SessionTarget::Serial(_) => ("串口终端", "Serial terminal"),
            kerminal_domain::sessions::SessionTarget::Container(_) => {
                ("容器终端", "Container terminal")
            }
        };
        let session_id = info.session_id.as_str();
        let short_id = session_id
            .chars()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        (
            crate::locale::text(
                format!("{target_zh} · {short_id}"),
                format!("{target_en} · {short_id}"),
            ),
            if self.ui_session_binding.is_some() {
                crate::locale::text("当前填入目标", "Fill target")
            } else {
                crate::locale::text("选择分屏后可填入命令", "Choose a pane to fill commands")
            },
        )
    }
}

/// 为旧页状态明确标出其上次成功的分类和搜索词，避免用户误把它当作新结果。
///
/// @author kongweiguang
fn snippet_filter_description(
    section: SnippetCatalogSection,
    query: &str,
) -> gpui_kit::SharedString {
    let (section_zh, section_en) = match section {
        SnippetCatalogSection::Common => ("常用", "Common"),
        SnippetCatalogSection::Mine => ("我的", "Mine"),
        SnippetCatalogSection::Library => ("命令库", "Library"),
    };
    if query.trim().is_empty() {
        crate::locale::text(section_zh, section_en)
    } else {
        let query = query.trim();
        crate::locale::text(
            format!("{section_zh} · 搜索“{query}”"),
            format!("{section_en} · search “{query}”"),
        )
    }
}

/// 行尾标签与 main 一样显示内置片段分类或“我的”；tooltip 保留来源、范围、平台和 shell 信息。
///
/// @author kongweiguang
fn row_metadata(
    origin: SnippetCatalogOrigin,
    category: &str,
    scope: SnippetScope,
    platforms: &[String],
    shells: &[String],
) -> (gpui_kit::SharedString, gpui_kit::SharedString) {
    let (source_zh, source_en) = match origin {
        SnippetCatalogOrigin::User => ("我的", "Mine"),
        SnippetCatalogOrigin::Builtin => ("内置", "Built-in"),
    };
    let (scope_zh, scope_en) = match scope {
        SnippetScope::Any => ("通用", "Any"),
        SnippetScope::Local => ("本地", "Local"),
        SnippetScope::Ssh => ("SSH", "SSH"),
    };
    let platforms_zh = if platforms.is_empty() {
        "不限".to_owned()
    } else {
        platforms.join("、")
    };
    let platforms_en = if platforms.is_empty() {
        "Any".to_owned()
    } else {
        platforms.join(", ")
    };
    let shells_zh = if shells.is_empty() {
        "不限".to_owned()
    } else {
        shells.join("、")
    };
    let shells_en = if shells.is_empty() {
        "Any".to_owned()
    } else {
        shells.join(", ")
    };
    let (category_zh, category_en) = match origin {
        SnippetCatalogOrigin::User => ("我的".to_owned(), "Mine".to_owned()),
        SnippetCatalogOrigin::Builtin => (category.to_owned(), category.to_owned()),
    };
    (
        crate::locale::text(category_zh.clone(), category_en.clone()),
        crate::locale::text(
            format!(
                "分类：{category_zh} · 来源：{source_zh} · 范围：{scope_zh} · 平台：{platforms_zh} · Shell：{shells_zh}"
            ),
            format!(
                "Category: {category_en} · Source: {source_en} · Scope: {scope_en} · Platforms: {platforms_en} · Shells: {shells_en}"
            ),
        ),
    )
}

/// main 导入弹框的按钮受 font: inherit 影响，实窗为 16px/36px；内容单独定标以保留 Kit 的焦点和禁用行为。
///
/// @author kongweiguang
fn snippet_import_button(
    id: &'static str,
    label: gpui_kit::SharedString,
    icon: Option<IconName>,
    theme: ThemeTokens,
) -> Button {
    Button::new(id)
        .h(px(36.))
        .px(px(14.))
        .rounded(theme.control_radius)
        .accessibility_label(label.clone())
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(16.))
                .line_height(px(24.))
                .font_weight(gpui_kit::FontWeight::NORMAL)
                .when_some(icon, |row, icon| row.child(Icon::new(icon).size(px(16.))))
                .child(label),
        )
}

/// 来源前缀用于稳定组件 ID，用户与内置模板可以共享人类可读标题。
///
/// @author kongweiguang
fn origin_key(origin: SnippetCatalogOrigin) -> &'static str {
    match origin {
        SnippetCatalogOrigin::User => "user",
        SnippetCatalogOrigin::Builtin => "builtin",
    }
}

/// 分类索引只服务于本地 TabList 键盘导航，映射保持与 UI 顺序一致。
///
/// @author kongweiguang
fn section_index(section: SnippetCatalogSection) -> usize {
    match section {
        SnippetCatalogSection::Common => 0,
        SnippetCatalogSection::Mine => 1,
        SnippetCatalogSection::Library => 2,
    }
}

/// 键盘导航目标从固定的三个领域区段生成，不依赖列表项或分页内容。
///
/// @author kongweiguang
fn section_at(index: usize) -> SnippetCatalogSection {
    match index % 3 {
        0 => SnippetCatalogSection::Common,
        1 => SnippetCatalogSection::Mine,
        _ => SnippetCatalogSection::Library,
    }
}
