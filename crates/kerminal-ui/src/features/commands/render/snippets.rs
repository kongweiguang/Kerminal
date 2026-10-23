//! 片段目录浏览入口；编辑草稿通过单独弹层承载，目录保持紧凑可检索。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, Focusable as _, InteractiveElement as _, IntoElement, KeyDownEvent,
    StatefulInteractiveElement as _, Window,
    assets::IconName,
    component::{
        Disableable, Sizable as _,
        button::{Button, ButtonVariants},
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
    SnippetCatalogItem, SnippetCatalogOrigin, SnippetPreferenceKey, SnippetRisk, SnippetScope,
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
    /// 管理 flag 路由到独立配置页；搜索快捷键仅在片段子树内接管且不覆盖输入控件焦点。
    ///
    /// @author kongweiguang
    pub(super) fn render_snippets(
        &mut self,
        theme: ThemeTokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        if self.snippet_import_review.is_some() {
            return self
                .render_snippet_import_review(theme, cx)
                .into_any_element();
        }
        if self.snippet_library_manager_open {
            return self
                .render_snippet_library_manager(theme, cx)
                .into_any_element();
        }

        let toolbar = self.render_snippet_toolbar(theme, cx);
        let search = div()
            .id("snippet-catalog-search")
            .w_full()
            .px_3()
            .py_1()
            .child(Input::new(&self.snippet_query).w_full().h(px(32.)))
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
        let has_more = self.snippet_next_cursor.is_some() && !rows_stale && !has_error;

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
            .overflow_hidden()
            .bg(theme.materials.shell)
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
                    })
                    .when(has_more, |list| {
                        list.child(
                            div().w_full().flex().justify_center().p_2().child(
                                Button::new("snippet-catalog-load-more")
                                    .compact()
                                    .ghost()
                                    .label(crate::locale::text("加载更多", "Load more"))
                                    .disabled(loading)
                                    .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                        view.load_more_snippets(window, cx);
                                    })),
                            ),
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

    /// 导入确认只呈现安全摘要；结果未知时保留批次身份并只开放状态查询。
    ///
    /// @author kongweiguang
    fn render_snippet_import_review(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        use super::super::snippet_transfer_ui::SnippetImportPhase;

        let Some(review) = self.snippet_import_review.as_ref() else {
            return div().id("snippet-import-review-empty").into_any_element();
        };
        let phase = review.phase;
        let selected_count = review.selected_count();
        let candidates_empty = review.candidates.is_empty();
        let candidate_rows = review
            .candidates
            .iter()
            .map(|candidate| {
                let source_index = candidate.source_index;
                let selected = candidate.selected;
                let title = candidate.title.clone();
                let conflict = candidate.title_conflicts;
                let scope = import_scope_label(candidate.scope);
                let risk = import_risk_label(candidate.risk);
                let variable_count = candidate.variable_count;
                let toggle_label = if selected {
                    crate::locale::text("已选择", "Selected")
                } else {
                    crate::locale::text("选择导入", "Select")
                };
                div()
                    .id(format!("snippet-import-candidate-{source_index}"))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .when(selected, |row| row.bg(theme.selected.opacity(0.2)))
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
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_color(theme.foreground)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap_2()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(scope)
                                    .child(risk)
                                    .child(crate::locale::text(
                                        format!("{variable_count} 个变量"),
                                        format!("{variable_count} variables"),
                                    ))
                                    .when(conflict, |row| {
                                        row.child(crate::locale::text(
                                            "标题与已有片段重复",
                                            "Title conflicts with an existing snippet",
                                        ))
                                        .text_color(theme.warning)
                                    }),
                            ),
                    )
                    .child(
                        Button::new(format!("snippet-import-select-{source_index}"))
                            .compact()
                            .when(selected, Button::secondary)
                            .when(!selected, Button::ghost)
                            .accessibility_label(crate::locale::text(
                                if selected {
                                    "取消选择导入"
                                } else {
                                    "选择导入"
                                },
                                if selected {
                                    "Deselect import"
                                } else {
                                    "Select import"
                                },
                            ))
                            .label(toggle_label)
                            .disabled(!matches!(
                                phase,
                                SnippetImportPhase::Reviewing | SnippetImportPhase::NotCommitted
                            ))
                            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                view.set_snippet_import_candidate_selected(
                                    source_index,
                                    !selected,
                                    cx,
                                );
                            })),
                    )
            })
            .collect::<Vec<_>>();
        let file_name = review.file_name.clone();
        let issues = review.issues.clone();
        let status = review.status.clone();
        let can_change_selection = matches!(
            phase,
            SnippetImportPhase::Reviewing | SnippetImportPhase::NotCommitted
        );
        let can_close = !matches!(
            phase,
            SnippetImportPhase::Committing | SnippetImportPhase::OutcomeUnknown
        ) && !self.snippet_transfer_busy;
        let can_confirm = can_change_selection && selected_count > 0 && !self.snippet_transfer_busy;
        let can_reconcile =
            phase == SnippetImportPhase::OutcomeUnknown && !self.snippet_transfer_busy;

        div()
            .id("snippet-import-review")
            .size_full()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.materials.shell)
            .child(
                div()
                    .w_full()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_3()
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
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child(crate::locale::text("导入预览", "Import preview")),
                            )
                            .child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(file_name),
                            ),
                    )
                    .child(
                        Button::new("snippet-import-review-close")
                            .compact()
                            .label(crate::locale::text("关闭", "Close"))
                            .disabled(!can_close)
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.cancel_snippet_import_review(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("snippet-import-review-summary")
                    .w_full()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_2()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        format!("已选择 {selected_count} 个片段",),
                        format!("{selected_count} snippets selected",),
                    ))
                    .when(!can_change_selection, |bar| {
                        bar.child(import_phase_label(phase))
                    }),
            )
            .child(
                div()
                    .id("snippet-import-review-list")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(candidate_rows)
                    .when(issues.is_empty() && candidates_empty, |list| {
                        list.child(div().w_full().p_4().text_color(theme.muted).child(
                            crate::locale::text(
                                "没有可导入的有效片段。",
                                "No valid snippets are available to import.",
                            ),
                        ))
                    })
                    .children(issues.into_iter().enumerate().map(|(index, issue)| {
                        div()
                            .id(format!("snippet-import-issue-{index}"))
                            .w_full()
                            .px_4()
                            .py_2()
                            .text_size(theme.text_xs)
                            .text_color(theme.warning)
                            .child(issue)
                    }))
                    .when_some(status, |list, status| {
                        list.child(
                            div()
                                .id("snippet-import-review-status")
                                .w_full()
                                .px_4()
                                .py_2()
                                .text_color(theme.muted)
                                .child(status),
                        )
                    }),
            )
            .child(
                div()
                    .id("snippet-import-review-footer")
                    .w_full()
                    .flex_shrink_0()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.materials.chrome)
                    .when(can_reconcile, |footer| {
                        footer.child(
                            Button::new("snippet-import-reconcile")
                                .compact()
                                .primary()
                                .label(crate::locale::text("查询批次状态", "Check batch status"))
                                .disabled(!can_reconcile)
                                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                    view.reconcile_snippet_import(cx);
                                })),
                        )
                    })
                    .when(can_change_selection, |footer| {
                        footer.child(
                            Button::new("snippet-import-confirm")
                                .compact()
                                .primary()
                                .label(crate::locale::text(
                                    if phase == SnippetImportPhase::NotCommitted {
                                        "再次确认导入"
                                    } else {
                                        "导入所选片段"
                                    },
                                    if phase == SnippetImportPhase::NotCommitted {
                                        "Confirm import again"
                                    } else {
                                        "Import selected"
                                    },
                                ))
                                .disabled(!can_confirm)
                                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                    view.confirm_snippet_import(cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("snippet-import-review-cancel")
                            .compact()
                            .label(crate::locale::text("返回", "Back"))
                            .disabled(!can_close)
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.cancel_snippet_import_review(cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    /// 头部操作只调用已接入的目录用例，并返回拥有化元素以结束对 Commands 状态的渲染期借用。
    ///
    /// @author kongweiguang
    fn render_snippet_toolbar(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let target = self.snippet_current_target_label();
        let target_tooltip = target.clone();
        let transfer_available = self.snippet_transfer_files.is_some();
        let manager_available = self.configuration_directory.is_some();
        div()
            .id("snippet-catalog-toolbar")
            .w_full()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_1()
            .child(
                div()
                    .id("snippet-current-target")
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .aria_label(target.clone())
                    .tooltip(move |window, cx| {
                        Tooltip::new(target_tooltip.clone()).build(window, cx)
                    })
                    .child(target),
            )
            .child(
                Button::new("snippet-import")
                    .compact()
                    .ghost()
                    .w(px(32.))
                    .h(px(32.))
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
                    .w(px(32.))
                    .h(px(32.))
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
                    .w(px(32.))
                    .h(px(32.))
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
                    .w(px(32.))
                    .h(px(32.))
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

    /// 分类栏复用 Kit TabBar 的语义并补齐键盘导航；拥有化返回值让后续目录行构建可独立借用状态。
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

        div()
            .id("snippet-catalog-section-focus")
            .w_full()
            .px_3()
            .py_1()
            .track_focus(&self.snippet_catalog_focus)
            .tab_index(0)
            .tab_stop(true)
            .role(gpui_kit::Role::Group)
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
            .child(
                TabBar::new("snippet-catalog-sections")
                    .underline()
                    .small()
                    .selected_index(selected_index)
                    .w_full()
                    .h(px(32.))
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(theme.text_heading)
                    .children(tabs.into_iter().map(|(section, label)| {
                        let aria_label = label.clone();
                        Tab::new()
                            .flex_1()
                            .justify_center()
                            .label(label)
                            .aria_label(aria_label)
                            .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                                window.focus(&view.snippet_catalog_focus, cx);
                                view.select_snippet_catalog_section(section, window, cx);
                            }))
                    })),
            )
            .into_any_element()
    }

    /// 目录项以标题和说明为主；展开区仅复用已选定义、变量实体和唯一运行状态。
    /// 行本身自然增高，预览正文独立滚动，窄栏不会把执行确认挤出可见列表。
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
        let (metadata, metadata_details) =
            row_metadata(item.origin, item.scope, &item.platforms, &item.shells);
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

    /// 先显示与当前 UI binding 精确匹配的用户标签；投影缺失时才退回安全的协议和短会话标识。
    ///
    /// @author kongweiguang
    fn snippet_current_target_label(&self) -> gpui_kit::SharedString {
        if let Some(label) = self.session_target_label() {
            return crate::locale::text(
                format!("{label} · 当前填入目标"),
                format!("{label} · Fill target"),
            );
        }
        let Some(info) = self.session_info.as_ref() else {
            return crate::locale::text("未选择终端", "No terminal selected");
        };
        let target = match &info.target {
            kerminal_domain::sessions::SessionTarget::Local(_) => {
                crate::locale::text("本地终端", "Local terminal")
            }
            kerminal_domain::sessions::SessionTarget::Ssh(_) => {
                crate::locale::text("SSH 终端", "SSH terminal")
            }
            kerminal_domain::sessions::SessionTarget::Telnet(_) => {
                crate::locale::text("Telnet 终端", "Telnet terminal")
            }
            kerminal_domain::sessions::SessionTarget::Serial(_) => {
                crate::locale::text("串口终端", "Serial terminal")
            }
            kerminal_domain::sessions::SessionTarget::Container(_) => {
                crate::locale::text("容器终端", "Container terminal")
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
        crate::locale::text(
            format!("当前填入目标 · {target} · {short_id}"),
            format!("Fill target · {target} · {short_id}"),
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

/// 紧凑行只标注定义范围；平台与 shell 声明仅供参考，不把元数据伪装成直接发送的限制。
///
/// @author kongweiguang
fn row_metadata(
    origin: SnippetCatalogOrigin,
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
    (
        crate::locale::text(scope_zh, scope_en),
        crate::locale::text(
            format!(
                "来源：{source_zh} · 范围：{scope_zh} · 平台：{platforms_zh} · Shell：{shells_zh}"
            ),
            format!(
                "Source: {source_en} · Scope: {scope_en} · Platforms: {platforms_en} · Shells: {shells_en}"
            ),
        ),
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

/// 导入预览只映射用户可核对的类型标签，不暴露完整片段定义或变量默认值。
///
/// @author kongweiguang
fn import_scope_label(scope: SnippetScope) -> gpui_kit::SharedString {
    match scope {
        SnippetScope::Any => crate::locale::text("通用", "Any"),
        SnippetScope::Local => crate::locale::text("本地", "Local"),
        SnippetScope::Ssh => crate::locale::text("SSH", "SSH"),
    }
}

/// 风险摘要保留领域分类，未知风险不会被简化为安全状态。
///
/// @author kongweiguang
fn import_risk_label(risk: SnippetRisk) -> gpui_kit::SharedString {
    match risk {
        SnippetRisk::Inspect => crate::locale::text("只读", "Inspect"),
        SnippetRisk::Change => crate::locale::text("变更", "Change"),
        SnippetRisk::Destructive => crate::locale::text("破坏性", "Destructive"),
        SnippetRisk::Unknown => crate::locale::text("未知风险", "Unknown risk"),
    }
}

/// 批次阶段向用户说明下一步，不把 Unknown 描述成失败或可重发。
///
/// @author kongweiguang
fn import_phase_label(
    phase: super::super::snippet_transfer_ui::SnippetImportPhase,
) -> gpui_kit::SharedString {
    use super::super::snippet_transfer_ui::SnippetImportPhase;

    match phase {
        SnippetImportPhase::Reviewing => crate::locale::text("待确认", "Ready to review"),
        SnippetImportPhase::Committing => crate::locale::text("正在提交", "Importing"),
        SnippetImportPhase::NotCommitted => {
            crate::locale::text("尚未提交，可再次确认", "Not committed; review again")
        }
        SnippetImportPhase::OutcomeUnknown => crate::locale::text(
            "结果未知，请查询原批次",
            "Outcome unknown; check this batch",
        ),
        SnippetImportPhase::Committed => crate::locale::text("已导入", "Imported"),
        SnippetImportPhase::Failed => crate::locale::text("未能导入", "Import failed"),
    }
}
