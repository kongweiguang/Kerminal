//! 文件宿主统一绘制文件上下文行与唯一内部标签带；目录与编辑器正文保留各自实体。
//!
//! @author kongweiguang

use std::collections::{BTreeSet, HashMap};

use super::*;
use crate::theme::ThemeTokens;
use gpui_kit::component::{
    IconName, Selectable as _,
    button::{Button, ButtonVariants as _},
    menu::ContextMenuExt as _,
};
use gpui_kit::{AnyElement, ClickEvent, IntoElement, div, prelude::*, px};

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
        // 中间标签只承载文档；目录浏览属于右侧文件面板，避免在文件标签里再出现 SFTP 目录项。
        let buttons = state
            .items
            .iter()
            .filter_map(|item| {
                let FileWorkspaceItem::Document { .. } = item else {
                    return None;
                };
                let item_id = item.id().clone();
                let owner = id.clone();
                let close_owner = owner.clone();
                let close_id = item_id.clone();
                let activate_selector = format!("host-file-item-{}", item_id.as_str());
                let close_selector = format!("host-file-close-{}", item_id.as_str());
                let selected = effective_active == Some(&item_id);
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
                let name = host_document_label(item, &paths_by_title, local);
                // 脏标记必须固定在最前，否则长标题被省略号截断时未保存状态会消失。
                let label = if dirty {
                    format!("● {name}")
                } else {
                    name.clone()
                };
                let close_label = format!("{}{}", crate::locale::text("关闭 ", "Close "), name);
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
                            .icon(IconName::FileText)
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
                            .accessibility_label(close_label.clone())
                            .tooltip(close_label)
                            .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                root.close_host_file_item(&close_owner, &close_id, window, cx);
                            })),
                    );
                Some(if let Some((editor, endpoint, generation)) = menu_target {
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
                })
            })
            .collect::<Vec<_>>();
        let active_directory = effective_active.and_then(|active| {
            self.workspace_files
                .directories
                .get(&(id.clone(), active.clone()))
                .map(|(pane, _)| pane.clone())
        });
        // 宿主是唯一绘制方：先切换嵌入模式，再让上下文行和正文按同一帧状态组装，避免编辑器
        // 与宿主各画一份工具栏。
        if let Some(editor) = self.file_editor_workspace.clone()
            && !editor.read(cx).is_embedded_navigation()
        {
            editor.update(cx, |editor, cx| editor.set_embedded_navigation(true, cx));
        }
        // 活动文档只从已提交状态计算一次，上下文行和正文共用结论，避免同帧两处判断漂移。
        let active_document = self.workspace_files.host_active_document(id);
        // 文件上下文行固定位于文档标签带上方：文档活动时复用编辑器的保存工具栏，目录活动
        // 时用同规格的宿主/目录说明占位。目录没有文档标签时不画这一行，避免空标签带下多出
        // 一条无对象说明。
        let context_toolbar = match self.file_editor_workspace.clone() {
            Some(editor) if active_document.is_some() => {
                Some(editor.update(cx, |editor, cx| editor.render_context_toolbar(window, cx)))
            }
            Some(editor) if !buttons.is_empty() => self
                .workspace_files
                .host_active_directory_endpoint(id)
                .map(|endpoint| {
                    editor.update(cx, |editor, cx| {
                        editor.render_directory_context(&endpoint, window, cx)
                    })
                }),
            _ => None,
        };
        let body = if let Some(pane) = active_directory {
            pane.into_any_element()
        } else if active_document.is_some() {
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
        // 上下文行在最上，文档标签带紧随其下，最后是正文；目录不进入标签带。
        let tab_strip = if buttons.is_empty() {
            None
        } else {
            Some(
                div()
                    .h(theme.metrics.toolbar_height)
                    .w_full()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .bg(theme.panel)
                    .border_b_1()
                    .border_color(theme.border)
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
                    ),
            )
        };
        div()
            .id(format!("host-file-workspace-{}", id.as_str()))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.materials.workspace)
            .text_color(theme.foreground)
            .when_some(context_toolbar, |view, toolbar| view.child(toolbar))
            .when_some(tab_strip, |view, strip| view.child(strip))
            .child(div().flex_1().min_h(px(0.)).min_w(px(0.)).child(body))
            .into_any_element()
    }
}
