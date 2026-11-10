//! 文件宿主只画一条内部标签带；目录与编辑器正文保留各自实体。
//!
//! @author kongweiguang

use std::collections::{BTreeSet, HashMap};

use super::*;
use crate::theme::ThemeTokens;
use gpui_kit::component::{
    Disableable as _, IconName, Selectable as _,
    button::{Button, ButtonVariants as _},
    menu::ContextMenuExt as _,
};
use gpui_kit::{AnyElement, ClickEvent, IntoElement, div, prelude::*, px};

/// 目录标签只显示基名与最短唯一后缀；同路径的加号副本用稳定序号区分，完整路径留给 tooltip。
///
/// @author kongweiguang
fn host_directory_labels(entries: &[(&TabId, &str)], local: bool) -> HashMap<TabId, String> {
    let parts = |path: &str| {
        if local {
            std::path::Path::new(path)
                .iter()
                .map(|part| part.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        } else {
            path.split('/')
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        }
    };
    let separator = if local {
        std::path::MAIN_SEPARATOR_STR
    } else {
        "/"
    };
    let mut by_base = HashMap::<String, Vec<(&TabId, Vec<String>)>>::new();
    for (id, path) in entries {
        let components = parts(path);
        let base = components
            .last()
            .cloned()
            .filter(|part| !part.is_empty())
            .unwrap_or_else(|| path.to_string());
        by_base.entry(base).or_default().push((*id, components));
    }
    let mut labels = HashMap::new();
    for (base, group) in by_base {
        if group.len() == 1 {
            labels.insert(group[0].0.clone(), base);
            continue;
        }
        let max_depth = group
            .iter()
            .map(|(_, components)| components.len())
            .max()
            .unwrap_or(1)
            .max(2);
        let mut resolved = None;
        for depth in 2..=max_depth {
            let suffixes = group
                .iter()
                .map(|(_, components)| {
                    components[components.len().saturating_sub(depth)..].join(separator)
                })
                .collect::<BTreeSet<_>>();
            if suffixes.len() == group.len() {
                resolved = Some(depth);
                break;
            }
        }
        // 完全相同的路径无法靠后缀区分；按稳定项序补序号，避免关闭/切换时无对象可辨。
        let mut same_path_seen = HashMap::<String, usize>::new();
        for (id, components) in group {
            let label = if let Some(depth) = resolved {
                components[components.len().saturating_sub(depth)..].join(separator)
            } else {
                let whole = components.join(separator);
                let ordinal = same_path_seen.entry(whole).or_insert(0);
                *ordinal += 1;
                if *ordinal == 1 {
                    base.clone()
                } else {
                    format!("{base} · {ordinal}")
                }
            };
            labels.insert(id.clone(), label);
        }
    }
    labels
}

/// 同名文档复用本帧分组，仍按完整同名集合求最短唯一后缀；远端反斜杠不是分隔符。
/// @author kongweiguang
fn host_document_label(
    item: &FileWorkspaceItem,
    paths_by_title: &HashMap<&str, Vec<&str>>,
    local: bool,
) -> String {
    let FileWorkspaceItem::Document { title, path, .. } = item else {
        return item.path().to_owned();
    };
    let paths = paths_by_title
        .get(title.as_str())
        .map(Vec::as_slice)
        .unwrap_or_default();
    if paths.len() <= 1 {
        return title.clone();
    }
    let parts = |path: &str| {
        if local {
            std::path::Path::new(path)
                .iter()
                .map(|part| part.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        } else {
            path.split('/')
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        }
    };
    let separator = if local {
        std::path::MAIN_SEPARATOR_STR
    } else {
        "/"
    };
    let components = paths.iter().map(|path| parts(path)).collect::<Vec<_>>();
    let current = parts(path);
    for depth in 2..=components.iter().map(Vec::len).max().unwrap_or(1) {
        let suffixes = components
            .iter()
            .map(|parts| parts[parts.len().saturating_sub(depth)..].join(separator))
            .collect::<BTreeSet<_>>();
        if suffixes.len() == paths.len() {
            return format!(
                "{title} · {}",
                current[current.len().saturating_sub(depth)..].join(separator)
            );
        }
    }
    format!("{title} · {path}")
}

impl RootView {
    /// 稳定 item ID 绑定操作；选中底色覆盖整项，使关闭按钮归属明确且保持独立交互。
    /// @author kongweiguang
    pub(in crate::root) fn render_host_file_workspace(
        &mut self,
        tab: &WorkspaceSessionTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let WorkspaceSessionTab::FileWorkspace { id, state, .. } = tab else {
            return div().into_any_element();
        };
        let theme = ThemeTokens::current(window, cx);
        let projection = self
            .file_editor_workspace
            .as_ref()
            .map(|editor| editor.read(cx).workspace_projection());
        let effective_active = state.active_item_id.as_ref().or_else(|| {
            state
                .items
                .iter()
                .find(|item| matches!(item, FileWorkspaceItem::Directory { .. }))
                .map(FileWorkspaceItem::id)
        });
        let directory_count = state
            .items
            .iter()
            .filter(|item| matches!(item, FileWorkspaceItem::Directory { .. }))
            .count();
        // 同名判断只服务本帧显示，完整端点和读取代次仍由原运行态校验。
        let mut paths_by_title = HashMap::<&str, Vec<&str>>::new();
        for item in &state.items {
            if let FileWorkspaceItem::Document { title, path, .. } = item {
                paths_by_title
                    .entry(title.as_str())
                    .or_default()
                    .push(path.as_str());
            }
        }
        let local = matches!(
            self.workspace_files.host_workspace_anchor(id),
            Some(FileEndpoint::Local { .. })
        );
        let directory_entries = state
            .items
            .iter()
            .filter_map(|item| match item {
                FileWorkspaceItem::Directory { id, path } => Some((id, path.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        let directory_labels = host_directory_labels(&directory_entries, local);
        let buttons = state
            .items
            .iter()
            .map(|item| {
                let item_id = item.id().clone();
                let owner = id.clone();
                let close_owner = owner.clone();
                let close_id = item_id.clone();
                let activate_selector = format!("host-file-item-{}", item_id.as_str());
                let close_selector = format!("host-file-close-{}", item_id.as_str());
                let selected = effective_active == Some(&item_id);
                let (name, dirty) = match item {
                    FileWorkspaceItem::Directory { path, .. } => (
                        directory_labels
                            .get(&item_id)
                            .cloned()
                            .unwrap_or_else(|| path.clone()),
                        false,
                    ),
                    FileWorkspaceItem::Document { .. } => {
                        let dirty = self
                            .workspace_files
                            .host_document_endpoint(id, item.id())
                            .is_some_and(|endpoint| {
                                projection.as_ref().is_some_and(|projection| {
                                    projection
                                        .tabs
                                        .iter()
                                        .any(|tab| tab.endpoint == endpoint && tab.dirty)
                                })
                            });
                        (host_document_label(item, &paths_by_title, local), dirty)
                    }
                };
                // 脏标记必须固定在最前，否则长标题被省略号截断时未保存状态会消失。
                let label = if dirty {
                    format!("● {name}")
                } else {
                    name.clone()
                };
                let close_label = format!("{}{}", crate::locale::text("关闭 ", "Close "), name);
                let close_tooltip = if matches!(item, FileWorkspaceItem::Directory { .. })
                    && directory_count <= 1
                {
                    crate::locale::text("至少保留一个目录标签", "Keep at least one directory tab")
                        .to_string()
                } else {
                    close_label.clone()
                };
                let tooltip = item.path().to_owned();
                let menu_target = self.file_editor_workspace.as_ref().and_then(|editor| {
                    let endpoint = self.workspace_files.host_document_endpoint(id, item.id())?;
                    let generation = editor.read(cx).load_generation_for(&endpoint)?;
                    Some((editor.clone(), endpoint, generation))
                });
                let tab = div()
                    .id(format!("file-workspace-item-{}", item_id.as_str()))
                    .h_full()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .max_w(window.rem_size() * 16.)
                    .overflow_hidden()
                    .bg(if selected {
                        theme.selected
                    } else {
                        theme.panel
                    })
                    .child(
                        Button::new(format!("file-workspace-activate-{}", item_id.as_str()))
                            .debug_selector(move || activate_selector.clone())
                            .flex_1()
                            .min_w(px(0.))
                            .compact()
                            .ghost()
                            .selected(selected)
                            .toggled(selected)
                            .icon(if matches!(item, FileWorkspaceItem::Directory { .. }) {
                                IconName::FolderOpen
                            } else {
                                IconName::FileText
                            })
                            .label(label)
                            .tooltip(tooltip)
                            .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
                                root.activate_host_file_item(&owner, &item_id, window, cx);
                            })),
                    )
                    .child(
                        Button::new(format!("file-workspace-close-{}", close_id.as_str()))
                            .debug_selector(move || close_selector.clone())
                            .flex_shrink_0()
                            .compact()
                            .ghost()
                            .icon(IconName::Close)
                            .disabled(
                                matches!(item, FileWorkspaceItem::Directory { .. })
                                    && directory_count <= 1,
                            )
                            .accessibility_label(close_label)
                            .tooltip(close_tooltip)
                            .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                root.close_host_file_item(&close_owner, &close_id, window, cx);
                            })),
                    );
                if let Some((editor, endpoint, generation)) = menu_target {
                    tab.context_menu(move |menu, _, _| {
                        crate::features::editor::editor_tab_context_menu(
                            menu,
                            editor.clone(),
                            endpoint.clone(),
                            generation,
                        )
                    })
                    .into_any_element()
                } else {
                    tab.into_any_element()
                }
            })
            .collect::<Vec<_>>();
        let add_owner = id.clone();
        let locate_owner = id.clone();
        // 目录态没有可定位的文档，禁用以避免点击后静默无效。
        let locate_available =
            self.workspace_files.host_active_document(id).is_some() && self.sftp_panel.is_some();
        let active_directory = effective_active.and_then(|active| {
            self.workspace_files
                .directories
                .get(&(id.clone(), active.clone()))
                .map(|(pane, _)| pane.clone())
        });
        let body = if let Some(pane) = active_directory {
            pane.into_any_element()
        } else if self.workspace_files.host_active_document(id).is_some() {
            self.file_editor_workspace
                .clone()
                .map(|editor| editor.into_any_element())
                .unwrap_or_else(|| {
                    div()
                        .child(crate::locale::text(
                            "文件编辑服务不可用",
                            "The file editor is unavailable",
                        ))
                        .into_any_element()
                })
        } else {
            let retry_id = id.clone();
            let message = match self.workspace_files.restore_state(id) {
                Some(WorkspaceFileRestoreState::Failed { message, .. }) => message.clone(),
                _ => crate::locale::text(
                    "目录尚未就绪，请重试。",
                    "The directory is unavailable. Try again.",
                ),
            };
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(message)
                .child(
                    Button::new("file-workspace-retry")
                        .label(crate::locale::text("重试", "Retry"))
                        .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
                            root.retry_workspace_file_tab(&retry_id, window, cx);
                            root.activate_host_file_workspace(&retry_id, window, cx);
                        })),
                )
                .into_any_element()
        };
        div()
            .id(format!("host-file-workspace-{}", id.as_str()))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.materials.workspace)
            .text_color(theme.foreground)
            .child(
                div()
                    .h(theme.metrics.toolbar_height)
                    .w_full()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .bg(theme.panel)
                    .child(
                        div()
                            .id(format!("host-file-items-{}", id.as_str()))
                            .debug_selector(|| "host-file-tab-strip".into())
                            .flex_1()
                            .min_w(px(0.))
                            .h_full()
                            .flex()
                            .gap(px(2.))
                            .overflow_x_scroll()
                            .when_some(
                                self.workspace_files
                                    .host_tab_scroll_handles
                                    .get(id)
                                    .cloned(),
                                |view, handle| view.track_scroll(&handle),
                            )
                            .children(buttons),
                    )
                    .child(
                        // 尾部动作与可滚动标签带用分隔线分组，窄窗口下也保持入口归属清楚。
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .pl_1()
                            .border_l_1()
                            .border_color(theme.border)
                            .child(
                                Button::new("file-workspace-new-directory")
                                    .debug_selector(|| "host-file-new-directory".into())
                                    .compact()
                                    .ghost()
                                    .icon(IconName::Plus)
                                    .accessibility_label(crate::locale::text(
                                        "新建目录标签",
                                        "New directory tab",
                                    ))
                                    .tooltip(crate::locale::text(
                                        "新建目录标签",
                                        "New directory tab",
                                    ))
                                    .on_click(cx.listener(
                                        move |root, _: &ClickEvent, window, cx| {
                                            root.new_host_directory_tab(&add_owner, window, cx);
                                        },
                                    )),
                            )
                            .child(
                                Button::new("file-workspace-locate-file")
                                    .debug_selector(|| "host-file-locate-file".into())
                                    .compact()
                                    .label(crate::locale::text("定位文件", "Locate file"))
                                    .disabled(!locate_available)
                                    .tooltip(if locate_available {
                                        crate::locale::text(
                                            "在右侧文件面板定位当前文件",
                                            "Locate the current file in the right panel",
                                        )
                                    } else {
                                        crate::locale::text(
                                            "先打开一个文件才能定位",
                                            "Open a file to locate it",
                                        )
                                    })
                                    .on_click(cx.listener(move |root, _: &ClickEvent, _, cx| {
                                        if let Some(endpoint) =
                                            root.workspace_files.host_active_document(&locate_owner)
                                            && let Some(parent) =
                                                crate::features::files::parent_endpoint(&endpoint)
                                            && let Some(pane) = root.sftp_panel.clone()
                                        {
                                            pane.update(cx, |pane, cx| {
                                                pane.locate_file_workspace_directory(parent, cx)
                                            });
                                        }
                                    })),
                            ),
                    ),
            )
            .child(div().flex_1().min_h(px(0.)).min_w(px(0.)).child(body))
            .into_any_element()
    }
}
