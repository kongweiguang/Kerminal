//! 文件编辑器视图绘制与用户操作入口。
//!
//! @author kongweiguang

use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::{
    AnyElement, ClickEvent, Context, Entity, Focusable as _, IntoElement, Render, ScrollHandle,
    StatefulInteractiveElement as _, Window,
    base::Selectable as _,
    component::{
        Disableable as _, IconName,
        button::{Button, ButtonVariants as _},
        input::{Editor, Input},
        menu::ContextMenuExt as _,
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::files::FileEndpoint;
use kerminal_domain::files::editor::{EditorPhase, SaveResolution};

use crate::theme::ThemeTokens;
use crate::ui_button::icon_button;

use super::context_menu::{editor_context_menu, editor_tab_context_menu};
use super::search_model::EditorSearchOption;
use super::workspace::{FileEditorWorkspace, endpoint_title};
use crate::features::files::FileTargetNameState;

/// 保存按钮在本帧展示所需的状态快照，避免异步保存回执字段分散在绘制参数中。
///
/// @author kongweiguang
struct SaveToolbarState {
    label: String,
    can_save: bool,
    saving: bool,
    cancel_requested: bool,
    endpoint: Option<kerminal_domain::files::FileEndpoint>,
}

/// 只有路径本身不能区分标签时才追加目标名称，完整稳定 ID 留在详情与可访问标签。
///
/// @author kongweiguang
fn disambiguated_tab_titles_with_names(
    tabs: &[(&FileEndpoint, &str)],
    target_names: &FileTargetNameState,
) -> Vec<String> {
    let mut titles = tabs
        .iter()
        .map(|(_, title)| (*title).to_owned())
        .collect::<Vec<_>>();
    let mut groups = BTreeMap::<String, Vec<usize>>::new();
    let path_displays = tabs
        .iter()
        .map(|(endpoint, _)| endpoint_path_display(endpoint))
        .collect::<Vec<_>>();

    for (index, (endpoint, _)) in tabs.iter().enumerate() {
        groups
            .entry(endpoint_title(endpoint))
            .or_default()
            .push(index);
    }

    for (basename, indices) in groups.into_iter().filter(|(_, indices)| indices.len() > 1) {
        let maximum_depth = indices
            .iter()
            .map(|index| path_displays[*index].components.len())
            .max()
            .unwrap_or(1);
        let unique_suffixes = (2..=maximum_depth)
            .map(|depth| {
                indices
                    .iter()
                    .map(|index| path_displays[*index].tail(depth))
                    .collect::<Vec<_>>()
            })
            .find(|suffixes| suffixes.iter().collect::<BTreeSet<_>>().len() == suffixes.len())
            .unwrap_or_else(|| {
                indices
                    .iter()
                    .map(|index| {
                        format!(
                            "{} · {}",
                            path_displays[*index].full_path,
                            endpoint_source_identity(tabs[*index].0, target_names)
                        )
                    })
                    .collect()
            });

        for (index, suffix) in indices.into_iter().zip(unique_suffixes) {
            titles[index] = if tabs[index].1 == basename {
                suffix
            } else {
                format!("{} · {suffix}", titles[index])
            };
        }
    }

    titles
}

/// 按本机平台或远端 POSIX 规则提取路径片段，不把远端路径交给宿主系统解析。
///
/// @author kongweiguang
fn endpoint_path_display(endpoint: &FileEndpoint) -> EndpointPathDisplay {
    match endpoint {
        FileEndpoint::Local { path } => EndpointPathDisplay {
            components: path
                .iter()
                .map(|component| component.to_string_lossy().into_owned())
                .collect(),
            full_path: path.display().to_string(),
            separator: std::path::MAIN_SEPARATOR,
        },
        FileEndpoint::Remote { path, .. } | FileEndpoint::Container { path, .. } => {
            EndpointPathDisplay {
                components: path
                    .as_str()
                    .split('/')
                    .filter(|component| !component.is_empty())
                    .map(str::to_owned)
                    .collect(),
                full_path: path.as_str().to_owned(),
                separator: '/',
            }
        }
    }
}

/// 路径尾部优先使用相对短路径，只有片段仍冲突时才返回完整原始路径。
///
/// @author kongweiguang
struct EndpointPathDisplay {
    components: Vec<String>,
    full_path: String,
    separator: char,
}

impl EndpointPathDisplay {
    /// 保留 Windows 盘符、根路径和远端 POSIX 根路径，避免手工推测目标根目录。
    ///
    /// @author kongweiguang
    fn tail(&self, depth: usize) -> String {
        if self.components.is_empty() || depth >= self.components.len() {
            return self.full_path.clone();
        }

        self.components[self.components.len() - depth..].join(&self.separator.to_string())
    }
}

/// 给完整目标提示与无障碍名称附带稳定主机或容器身份，不输出认证配置。
///
/// @author kongweiguang
fn endpoint_target_label(endpoint: &FileEndpoint, target_names: &FileTargetNameState) -> String {
    target_names.endpoint_detail(endpoint)
}

/// 编辑器头部优先展示连接名与文件名，完整身份和路径留给 tooltip 与无障碍信息；仅移除
/// 本投影添加的 SSH 前缀，用户保存的显示名保持原样。
///
/// @author kongweiguang
fn editor_visible_context_label(endpoint: &FileEndpoint, compact_target_label: &str) -> String {
    let target = match endpoint {
        FileEndpoint::Remote { .. } => compact_target_label
            .strip_prefix("SSH · ")
            .filter(|name| !name.is_empty())
            .unwrap_or(compact_target_label),
        _ => compact_target_label,
    };
    let path_display = endpoint_path_display(endpoint);
    let basename = path_display
        .components
        .last()
        .cloned()
        .unwrap_or(path_display.full_path);

    if target.is_empty() {
        basename
    } else if basename.is_empty() {
        target.to_owned()
    } else {
        format!("{target} · {basename}")
    }
}

/// 仅在路径尾部完全相同的跨目标文件之间补充目标身份，避免短标签泄露连接细节。
///
/// @author kongweiguang
fn endpoint_source_identity(endpoint: &FileEndpoint, target_names: &FileTargetNameState) -> String {
    target_names.display(endpoint).detail
}

impl Render for FileEditorWorkspace {
    /// 外壳保持不透明正文；标签关闭使用通用图标但仍绑定 endpoint 与读取代次，沿用未保存保护。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        self.update_search_decoration_theme(theme, cx);
        schedule_active_tab_reveal_probe(
            window,
            cx.entity().clone(),
            self.tabs_scroll_handle.clone(),
        );
        let tab_inputs = self
            .tabs()
            .iter()
            .map(|tab| (&tab.endpoint, tab.title.as_str()))
            .collect::<Vec<_>>();
        let tab_titles = disambiguated_tab_titles_with_names(&tab_inputs, &self.target_names);
        let tabs = self
            .tabs()
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let endpoint = tab.endpoint.clone();
                let active = self.active_endpoint() == Some(&tab.endpoint);
                let close_endpoint = endpoint.clone();
                let action_endpoint = endpoint.clone();
                let load_generation = tab.load_generation;
                let tab_workspace = cx.entity().clone();
                let tab_title = tab_titles
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| tab.title.clone());
                let display_title = if tab.dirty {
                    format!("● {tab_title}")
                } else {
                    tab_title
                };
                let target_label = endpoint_target_label(&tab.endpoint, &self.target_names);
                let tooltip_title = format!("{display_title}\n{target_label}");
                let accessibility_label = crate::locale::text(
                    format!("打开文件 {}，{}", display_title, target_label),
                    format!("Open file {}, {}", display_title, target_label),
                );
                let close_label = crate::locale::text(
                    format!("关闭文件 {}，{}", display_title, target_label),
                    format!("Close file {}, {}", display_title, target_label),
                );
                div()
                    .id(format!("file-editor-tab-{index}"))
                    .h_full()
                    .max_w(px(240.))
                    .flex_shrink_0()
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_t_md()
                    .bg(if active {
                        theme.background
                    } else {
                        theme.panel
                    })
                    .text_color(if active {
                        theme.foreground
                    } else {
                        theme.muted
                    })
                    .cursor_pointer()
                    .aria_label(accessibility_label)
                    .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                        workspace.activate(endpoint.clone(), cx);
                    }))
                    .context_menu(move |menu, _, _| {
                        editor_tab_context_menu(
                            menu,
                            tab_workspace.clone(),
                            action_endpoint.clone(),
                            load_generation,
                        )
                    })
                    .child(
                        div()
                            .id(format!("file-editor-tab-title-{index}"))
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .tooltip(move |window, cx| {
                                Tooltip::new(tooltip_title.clone()).build(window, cx)
                            })
                            .child(display_title),
                    )
                    .child(
                        icon_button(
                            format!("file-editor-close-{index}"),
                            IconName::Close,
                            close_label.clone(),
                        )
                        .tooltip(close_label)
                        .on_click(cx.listener(
                            move |workspace, _: &ClickEvent, _, cx| {
                                // 关闭是独立动作；取消未保存确认时不应顺带激活被关闭标签。
                                cx.stop_propagation();
                                workspace.request_close(close_endpoint.clone(), cx);
                            },
                        )),
                    )
            })
            .collect::<Vec<_>>();

        let active = self.active_tab().map(|tab| {
            (
                tab.endpoint.clone(),
                tab.error.clone(),
                tab.session
                    .as_ref()
                    .map(|session| (session.phase(), session.is_truncated(), tab.is_readonly())),
                tab.loading,
                tab.dirty,
                tab.save_operation
                    .as_ref()
                    .is_some_and(|operation| operation.cancel_requested()),
            )
        });
        let can_save = active
            .as_ref()
            .is_some_and(|(_, _, session, loading, dirty, _)| {
                *dirty
                    && !*loading
                    && session.is_some_and(|(phase, _, readonly)| {
                        !readonly
                            && phase != EditorPhase::Saving
                            && phase != EditorPhase::Conflict
                            && phase != EditorPhase::OutcomeUnknown
                    })
            });
        let save_label = active
            .as_ref()
            .and_then(|(_, _, session, _, _, _)| *session)
            .map(|(phase, _, _)| match phase {
                EditorPhase::Saving => crate::locale::text("保存中…", "Saving…").to_string(),
                EditorPhase::Conflict => {
                    crate::locale::text("保存冲突", "Save conflict").to_string()
                }
                EditorPhase::OutcomeUnknown => {
                    crate::locale::text("结果待确认", "Result unknown").to_string()
                }
                EditorPhase::Failed => crate::locale::text("重试保存", "Retry save").to_string(),
                EditorPhase::Ready => crate::locale::text("保存", "Save").to_string(),
            })
            .unwrap_or_else(|| crate::locale::text("保存", "Save").to_string());
        let saving = active
            .as_ref()
            .and_then(|(_, _, session, _, _, _)| *session)
            .is_some_and(|(phase, _, _)| phase == EditorPhase::Saving);
        let cancel_requested = active
            .as_ref()
            .is_some_and(|(_, _, _, _, _, requested)| *requested);
        let save_endpoint = active
            .as_ref()
            .map(|(endpoint, _, _, _, _, _)| endpoint.clone());

        let mut content = div()
            .id("file-editor-workspace")
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.materials.workspace)
            .text_color(theme.foreground)
            .child(self.render_toolbar(
                SaveToolbarState {
                    label: save_label,
                    can_save,
                    saving,
                    cancel_requested,
                    endpoint: save_endpoint,
                },
                theme,
                cx,
            ))
            .child(
                div()
                    .id("file-editor-tabs-scroll")
                    .h(px(38.))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .bg(theme.panel)
                    .overflow_x_scroll()
                    .track_scroll(&self.tabs_scroll_handle)
                    .children(tabs),
            );

        // 查询文本可跨文件保留，但没有已创建的编辑器时没有可操作目标，因此不显示无效输入与替换按钮。
        if self.search_visible() && self.active_tab().is_some_and(|tab| tab.editor.is_some()) {
            content = content.child(self.render_search_bar(theme, cx));
        }

        if let Some((endpoint, _, _, true, _, _)) = active.as_ref() {
            let endpoint = endpoint.clone();
            content = content.child(
                div()
                    .w_full()
                    .px_3()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .bg(theme.panel)
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        "正在读取文件；取消不会替换当前编辑内容。",
                        "Reading the file. Cancelling keeps the current editor contents.",
                    ))
                    .child(
                        Button::new("file-editor-cancel-load")
                            .compact()
                            .label(crate::locale::text("取消读取", "Cancel read"))
                            .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                                workspace.cancel_load(endpoint.clone(), cx);
                            })),
                    ),
            );
        }

        if let Some((endpoint, error, session_state, _loading, _dirty, _cancel_requested)) = active
        {
            if let Some(message) = error {
                content = content.child(self.render_error_banner(
                    endpoint.clone(),
                    message,
                    session_state.is_none(),
                    theme,
                    cx,
                ));
            }
            if let Some((phase, truncated, readonly)) = session_state {
                if truncated {
                    content = content.child(
                        div()
                            .w_full()
                            .px_3()
                            .py_2()
                            .bg(theme.panel)
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "文件超过 10 MiB，当前内容不完整且只读；请通过外部文件工具处理完整文件。",
                                "The file exceeds 10 MiB. This incomplete view is read-only; use a file tool to handle the full file.",
                            )),
                    );
                } else if readonly {
                    content = content.child(
                        div()
                            .w_full()
                            .px_3()
                            .py_2()
                            .bg(theme.panel)
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "此文件由文件系统标记为只读；仍可选择和复制内容。",
                                "The file system marks this file read-only. You can still select and copy its contents.",
                            )),
                    );
                }
                if phase == EditorPhase::Conflict {
                    content = content.child(self.render_conflict_banner(endpoint, theme, cx));
                } else if phase == EditorPhase::OutcomeUnknown {
                    content = content.child(self.render_unknown_result_banner(endpoint, theme, cx));
                }
            }
        }

        content = content.child(self.render_editor_body(theme, cx));
        if let Some(status) = self.status() {
            content = content.child(
                div()
                    .w_full()
                    .h(px(28.))
                    .px_3()
                    .flex()
                    .items_center()
                    .bg(theme.panel)
                    .text_size(px(12.))
                    .text_color(theme.muted)
                    .child(status.to_owned()),
            );
        }
        content
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/editor_tab_labels.rs"]
mod tab_label_tests;

/// 在布局后的下一帧检测实际标签带宽度；仅选中项变化或视口收窄时发出一次子项显露请求。
///
/// @author kongweiguang
fn schedule_active_tab_reveal_probe(
    window: &mut Window,
    workspace_entity: Entity<FileEditorWorkspace>,
    scroll_handle: ScrollHandle,
) {
    window.on_next_frame(move |window, app| {
        let viewport_width = scroll_handle.bounds().size.width.as_f32();
        if viewport_width <= 0.0 {
            return;
        }

        let mut width_changed = false;
        workspace_entity.update(app, |workspace, cx| {
            let previous_width = workspace.last_tabs_viewport_width;
            let width_shrank = previous_width.is_some_and(|previous| viewport_width < previous);
            let viewport_changed =
                previous_width.is_some_and(|previous| (viewport_width - previous).abs() > 0.25);
            if previous_width.is_none() || viewport_changed {
                workspace.last_tabs_viewport_width = Some(viewport_width);
            }

            let active_endpoint = workspace.active_endpoint.clone();
            let active_index = active_endpoint.as_ref().and_then(|endpoint| {
                workspace
                    .tabs
                    .iter()
                    .position(|tab| &tab.endpoint == endpoint)
            });
            let active = active_endpoint.zip(active_index);
            let Some((endpoint, index)) = active else {
                workspace.last_tabs_auto_revealed = None;
                width_changed = viewport_changed;
                return;
            };

            let active_changed = workspace.last_tabs_auto_revealed.as_ref().is_none_or(
                |(previous_endpoint, previous_index)| {
                    previous_endpoint != &endpoint || *previous_index != index
                },
            );
            if (active_changed || width_shrank) && !tab_is_fully_visible(&scroll_handle, index) {
                scroll_handle.scroll_to_item(index);
                cx.notify();
            }
            workspace.last_tabs_auto_revealed = Some((endpoint, index));
            width_changed = viewport_changed;
        });

        if width_changed {
            schedule_active_tab_reveal_probe(window, workspace_entity, scroll_handle);
        }
    });
}

/// 按 GPUI 的内容偏移核对活动标签完整位于视口内，避免已经可见时改变用户的滚动位置。
///
/// @author kongweiguang
fn tab_is_fully_visible(scroll_handle: &ScrollHandle, index: usize) -> bool {
    let viewport = scroll_handle.bounds();
    let offset = scroll_handle.offset().x;
    scroll_handle.bounds_for_item(index).is_some_and(|tab| {
        tab.left() + offset >= viewport.left() && tab.right() + offset <= viewport.right()
    })
}

impl FileEditorWorkspace {
    /// 顶部使用 chrome 材料，路径区域可收缩；查找入口仅在有实际编辑器时启用，避免无目标空操作。
    ///
    /// @author kongweiguang
    fn render_toolbar(
        &mut self,
        save: SaveToolbarState,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let SaveToolbarState {
            label: save_label,
            can_save,
            saving,
            cancel_requested,
            endpoint: save_endpoint,
        } = save;
        let has_active_editor = self.active_tab().is_some_and(|tab| tab.editor.is_some());
        let save_button = if saving {
            if let Some(endpoint) = save_endpoint {
                Button::new("file-editor-cancel-save")
                    .compact()
                    .flex_shrink_0()
                    .label(if cancel_requested {
                        crate::locale::text("等待保存结果…", "Waiting for save result…")
                    } else {
                        crate::locale::text("取消保存", "Cancel save")
                    })
                    .disabled(cancel_requested)
                    .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                        workspace.request_save_cancellation(&endpoint, cx);
                    }))
            } else {
                Button::new("file-editor-save")
                    .compact()
                    .primary()
                    .flex_shrink_0()
                    .label(save_label)
                    .disabled(true)
            }
        } else {
            Button::new("file-editor-save")
                .compact()
                .primary()
                .flex_shrink_0()
                .label(save_label)
                .disabled(!can_save)
                .on_click(cx.listener(|workspace, _: &ClickEvent, _, cx| {
                    workspace.save_active(SaveResolution::CompareRevision, false, false, cx);
                }))
        };
        let (path_label, target_accessibility_label) = self
            .active_tab()
            .map(|tab| {
                let compact_target_label = self.target_names.display(&tab.endpoint).compact;
                (
                    editor_visible_context_label(&tab.endpoint, &compact_target_label),
                    endpoint_target_label(&tab.endpoint, &self.target_names),
                )
            })
            .unwrap_or_else(|| {
                let placeholder =
                    crate::locale::text("选择文件开始编辑", "Open a file to start editing")
                        .to_string();
                (placeholder.clone(), placeholder)
            });
        let tooltip_label = target_accessibility_label.clone();
        div()
            .w_full()
            .h(px(42.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .bg(theme.materials.chrome)
            .child(
                div()
                    .id("file-editor-path-label")
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(12.))
                    .text_color(theme.muted)
                    .aria_label(target_accessibility_label)
                    .tooltip(move |window, cx| Tooltip::new(tooltip_label.clone()).build(window, cx))
                    .child(path_label),
            )
            .child(save_button)
            .when(saving && cancel_requested, |toolbar| {
                toolbar.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "取消请求已发送；系统仍等待文件操作的真实结果。",
                            "Cancellation was requested; the app is waiting for the file operation's actual result.",
                        )),
                )
            })
            .child(
                Button::new("file-editor-find")
                    .compact()
                    .flex_shrink_0()
                    .disabled(!has_active_editor)
                    .label(if !self.search_visible() {
                        crate::locale::text("查找替换", "Find and replace")
                    } else if self.replace_visible() {
                        crate::locale::text("收起替换", "Hide replace")
                    } else {
                        crate::locale::text("展开替换", "Show replace")
                    })
                    .accessibility_label(crate::locale::text(
                        "切换查找与替换面板",
                        "Toggle find and replace panel",
                    ))
                    .tooltip(crate::locale::text(
                        "Ctrl/Cmd+F 查找，Ctrl/Cmd+H 展开替换",
                        "Ctrl/Cmd+F finds; Ctrl/Cmd+H opens replace",
                    ))
                    .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                        workspace.toggle_search(window, cx);
                    })),
            )
    }

    /// 查找条允许窄栏换行，通用导航图标与终端搜索一致；替换及搜索选项保留能区分语义的文字。
    fn render_search_bar(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let replace_disabled = !self.can_replace_match();
        let match_count = self
            .search_results
            .as_ref()
            .map_or(0, |results| results.total_matches());
        let has_matches = match_count > 0;
        let search_busy = self.search_activity != super::search::SearchActivity::Idle;
        let options = self.search_options;
        let replace_visible = self.replace_visible();
        let has_query = !self.search_query.is_empty();
        let search_has_error = self.search_error.is_some();
        let search_feedback_accessible_label = if self.search_activity
            == super::search::SearchActivity::Idle
            && has_query
            && has_matches
            && !search_has_error
        {
            let current = self.search_match_index.map_or(0, |index| index + 1);
            Some(crate::locale::text(
                format!("第 {current} 个，共 {match_count} 个匹配"),
                format!("Match {current} of {match_count}"),
            ))
        } else {
            None
        };
        let search_feedback = match self.search_activity {
            super::search::SearchActivity::Scanning => {
                Some(crate::locale::text("正在搜索…", "Searching…"))
            }
            super::search::SearchActivity::Locating => {
                Some(crate::locale::text("正在定位匹配…", "Locating match…"))
            }
            super::search::SearchActivity::Replacing => Some(crate::locale::text(
                "正在准备替换…",
                "Preparing replacement…",
            )),
            super::search::SearchActivity::Idle => {
                if let Some(error) = self.search_error.as_deref() {
                    Some(crate::locale::text(
                        format!("正则表达式无效：{error}"),
                        format!("Invalid regular expression: {error}"),
                    ))
                } else if has_query && !has_matches {
                    Some(crate::locale::text("未找到匹配", "No matches"))
                } else if has_query {
                    let current = self.search_match_index.map_or(0, |index| index + 1);
                    Some(crate::locale::text(
                        format!("{current}/{match_count}"),
                        format!("{current}/{match_count}"),
                    ))
                } else {
                    None
                }
            }
        };
        let replace_row = if replace_visible {
            Some(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child(Input::new(self.replace_input()).flex_1().min_w(px(96.)))
                    .child(
                        Button::new("file-editor-replace-current")
                            .compact()
                            .flex_shrink_0()
                            .label(crate::locale::text("替换", "Replace"))
                            .accessibility_label(crate::locale::text(
                                "替换当前匹配",
                                "Replace current match",
                            ))
                            .tooltip(crate::locale::text(
                                "替换当前匹配项",
                                "Replace the current match",
                            ))
                            .disabled(replace_disabled)
                            .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                                let replacement = workspace
                                    .replace_input()
                                    .update(cx, |state, _| state.value().to_string());
                                workspace.replace_current(replacement, window, cx);
                            })),
                    )
                    .child(
                        Button::new("file-editor-replace-all")
                            .compact()
                            .flex_shrink_0()
                            .label(crate::locale::text("全部替换", "Replace all"))
                            .accessibility_label(crate::locale::text(
                                "替换全部匹配",
                                "Replace all matches",
                            ))
                            .tooltip(crate::locale::text("替换全部匹配项", "Replace every match"))
                            .disabled(replace_disabled)
                            .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                                let replacement = workspace
                                    .replace_input()
                                    .update(cx, |state, _| state.value().to_string());
                                workspace.replace_all(replacement, window, cx);
                            })),
                    ),
            )
        } else {
            None
        };
        div()
            .w_full()
            .px_3()
            .py_1()
            .flex()
            .flex_col()
            .gap_1()
            .bg(theme.materials.chrome)
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child(Input::new(self.find_input()).flex_1().min_w(px(96.)))
                    .child(
                        icon_button(
                            "file-editor-previous-match",
                            IconName::ArrowUp,
                            crate::locale::text("上一个匹配", "Previous match"),
                        )
                        .tooltip(crate::locale::text("上一个匹配", "Previous match"))
                        .disabled(!has_matches || search_busy)
                        .on_click(cx.listener(
                            |workspace, _: &ClickEvent, _, cx| {
                                workspace.move_search(false, cx);
                            },
                        )),
                    )
                    .child(
                        icon_button(
                            "file-editor-next-match",
                            IconName::ArrowDown,
                            crate::locale::text("下一个匹配", "Next match"),
                        )
                        .tooltip(crate::locale::text("下一个匹配", "Next match"))
                        .disabled(!has_matches || search_busy)
                        .on_click(cx.listener(
                            |workspace, _: &ClickEvent, _, cx| {
                                workspace.move_search(true, cx);
                            },
                        )),
                    )
                    .child(
                        icon_button(
                            "file-editor-close-search",
                            IconName::Close,
                            crate::locale::text("关闭查找", "Close find"),
                        )
                        .tooltip(crate::locale::text("关闭查找", "Close find"))
                        .on_click(cx.listener(
                            |workspace, _: &ClickEvent, _, cx| {
                                workspace.close_search(cx);
                            },
                        )),
                    )
                    .child(
                        Button::new("file-editor-search-match-case")
                            .compact()
                            .flex_shrink_0()
                            .label(crate::locale::text("Aa", "Aa"))
                            .accessibility_label(crate::locale::text("区分大小写", "Match case"))
                            .selected(options.match_case)
                            .tooltip(crate::locale::text("区分大小写", "Match case"))
                            .on_click(cx.listener(|workspace, _: &ClickEvent, _, cx| {
                                workspace.toggle_search_option(EditorSearchOption::MatchCase, cx);
                            })),
                    )
                    .child(
                        Button::new("file-editor-search-whole-word")
                            .compact()
                            .flex_shrink_0()
                            .label(crate::locale::text("词", "Word"))
                            .accessibility_label(crate::locale::text(
                                "匹配完整单词",
                                "Match whole word",
                            ))
                            .selected(options.whole_word)
                            .tooltip(crate::locale::text("匹配完整单词", "Match whole word"))
                            .on_click(cx.listener(|workspace, _: &ClickEvent, _, cx| {
                                workspace.toggle_search_option(EditorSearchOption::WholeWord, cx);
                            })),
                    )
                    .child(
                        Button::new("file-editor-search-regex")
                            .compact()
                            .flex_shrink_0()
                            .label(".*")
                            .accessibility_label(crate::locale::text(
                                "使用正则表达式",
                                "Use regular expressions",
                            ))
                            .selected(options.regex)
                            .tooltip(crate::locale::text(
                                "使用正则表达式",
                                "Use regular expressions",
                            ))
                            .on_click(cx.listener(|workspace, _: &ClickEvent, _, cx| {
                                workspace.toggle_search_option(EditorSearchOption::Regex, cx);
                            })),
                    )
                    .when_some(search_feedback, |row, feedback| {
                        row.child(
                            div()
                                .id("file-editor-search-feedback")
                                .role(gpui_kit::Role::Status)
                                .when_some(search_feedback_accessible_label, |status, label| {
                                    status.aria_label(label)
                                })
                                .px_1()
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(if search_has_error {
                                    theme.error
                                } else {
                                    theme.muted
                                })
                                .child(feedback),
                        )
                    }),
            )
            .when_some(replace_row, |panel, row| panel.child(row))
            .on_key_down(cx.listener(|workspace, event, window, cx| {
                workspace.handle_key_down(event, window, cx);
            }))
    }

    /// 首读失败显示安全错误和重试/关闭操作，避免二进制、过大或无权限文件落入空白假成功态。
    fn render_error_banner(
        &mut self,
        endpoint: FileEndpoint,
        message: String,
        can_retry: bool,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let retry_endpoint = endpoint.clone();
        div()
            .w_full()
            .px_3()
            .py_2()
            .flex()
            .items_center()
            .gap_2()
            .bg(theme.panel)
            .text_color(theme.error)
            .child(message)
            .when(can_retry, |banner| {
                banner.child(
                    Button::new("file-editor-retry-open")
                        .compact()
                        .label(crate::locale::text("重试", "Retry"))
                        .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                            workspace.open_file(retry_endpoint.clone(), cx);
                        })),
                )
            })
            .child(
                Button::new("file-editor-close-failed")
                    .compact()
                    .label(crate::locale::text("关闭文件", "Close file"))
                    .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                        workspace.request_close(endpoint.clone(), cx);
                    })),
            )
    }

    /// revision 冲突必须提供明确的重载和二次确认覆盖，禁止普通保存自动清掉远端修改。
    fn render_conflict_banner(
        &mut self,
        endpoint: FileEndpoint,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let reload_endpoint = endpoint.clone();
        div()
            .w_full()
            .px_3()
            .py_2()
            .flex()
            .items_center()
            .gap_2()
            .bg(theme.panel)
            .text_color(theme.error)
            .child(crate::locale::text(
                "远端文件已变化；先重载远端版本，或确认覆盖远端内容。",
                "The remote file changed. Reload the latest version or explicitly confirm an overwrite.",
            ))
            .child(
                Button::new("file-editor-reload-conflict")
                    .compact()
                    .label(crate::locale::text("重载远端", "Reload remote"))
                    .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                        workspace.request_reload(reload_endpoint.clone(), cx);
                    })),
            )
            .child(
                Button::new("file-editor-overwrite-conflict")
                    .compact()
                    .label(crate::locale::text("确认覆盖…", "Confirm overwrite…"))
                    .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                        workspace.request_overwrite(endpoint.clone(), cx);
                    })),
            )
    }

    /// 未知提交只提供显式读回入口，避免用户在结果未确认时重复写入或覆盖文件。
    fn render_unknown_result_banner(
        &mut self,
        endpoint: FileEndpoint,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .w_full()
            .px_3()
            .py_2()
            .flex()
            .items_center()
            .gap_2()
            .bg(theme.panel)
            .text_color(theme.error)
            .child(crate::locale::text(
                "保存结果尚未确认；重新读取可确认文件当前内容，期间不会自动重试保存。",
                "The save result is unconfirmed. Reload the file to check its current contents; saving will not be retried automatically.",
            ))
            .child(
                Button::new("file-editor-readback-unknown-save")
                    .compact()
                    .label(crate::locale::text("重新读取文件…", "Reload file…"))
                    .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                        workspace.request_reload(endpoint.clone(), cx);
                    })),
            )
    }

    /// 待确认期间正文只读作为根焦点挂载前防线；不改变文件权限或取消后的编辑能力。
    ///
    /// @author kongweiguang
    fn render_editor_body(&mut self, theme: ThemeTokens, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.active_tab() else {
            return div()
                .id("file-editor-empty")
                .flex_1()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "在文件面板中打开一个文本文件",
                    "Open a text file from the file panel.",
                ))
                .into_any_element();
        };
        if let Some(editor) = &tab.editor {
            let readonly = self.confirmation().is_some() || tab.loading || tab.is_readonly();
            let endpoint = tab.endpoint.clone();
            let endpoint_for_key = endpoint.clone();
            let editor_focus = editor.read(cx).focus_handle(cx);
            let can_edit = !readonly
                && tab.session.as_ref().is_some_and(|session| {
                    !matches!(
                        session.phase(),
                        EditorPhase::Conflict | EditorPhase::OutcomeUnknown
                    )
                });
            let can_save = can_edit
                && tab.dirty
                && tab.session.as_ref().is_some_and(|session| {
                    matches!(session.phase(), EditorPhase::Ready | EditorPhase::Failed)
                });
            let workspace = cx.entity().clone();
            let pane = div()
                .id("file-editor-pane")
                .flex_1()
                .w_full()
                .min_h(px(0.))
                .flex()
                .flex_col()
                .bg(theme.panel)
                .on_key_down(cx.listener(move |workspace, event, window, cx| {
                    workspace.handle_key_down(event, window, cx);
                }))
                .child(
                    Editor::new(editor)
                        .h_full()
                        .w_full()
                        .text_size(theme.text_body)
                        .appearance(false)
                        .bordered(false)
                        .readonly(readonly)
                        .aria_label(crate::locale::text(
                            format!("编辑文件 {}", endpoint_title(&endpoint_for_key)),
                            format!("Edit file {}", endpoint_title(&endpoint_for_key)),
                        )),
                );
            return pane
                .context_menu(move |menu, _, _| {
                    editor_context_menu(
                        menu,
                        editor_focus.clone(),
                        workspace.clone(),
                        can_edit,
                        can_save,
                    )
                })
                .into_any_element();
        }

        let message = if tab.loading {
            crate::locale::text("正在读取文件…", "Reading file…").to_string()
        } else {
            crate::locale::text("无法打开文件", "Could not open file").to_string()
        };
        div()
            .id("file-editor-loading")
            .flex_1()
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(theme.muted)
            .child(message)
            .into_any_element()
    }
}
