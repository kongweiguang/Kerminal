//! 文件工作区的 Kit 虚拟树绘制；行操作统一回到 FilesPane 既有用例。
//!
//! @author kongweiguang

use std::sync::Arc;

use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::{
    AnyElement, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement, KeyDownEvent,
    MouseButton, StatefulInteractiveElement as _, StyleRefinement,
    base::{Checkbox, CheckboxIndicator, Tree, TreeEntry, TreeEntryState},
    component::{
        Icon, IconName,
        menu::{ContextMenuExt as _, PopupMenuItem},
        scroll::ScrollableElement as _,
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::files::{DirectoryListing, FileEndpoint, FileEntry, FileEntryKind};

use crate::{locale, theme::ThemeTokens};

use super::super::{
    FilesPane,
    drag_drop::{FileDragPreview, FileTransferDrag},
    render::browser,
};
use crate::features::files::{
    selection::SelectionGesture,
    tree::{TreeNode, TreeNodeMap},
};

/// 拖拽选择与目录代次一起捕获，避免刷新后把旧选择解释为新目录中的文件。
struct TreeDragSource {
    directory: FileEndpoint,
    generation: u64,
    selected_entries: Arc<Vec<FileEntry>>,
    included_descendants: usize,
}

/// 树行共享的目录快照、pane 目标和样式集中保存，避免回调位置参数错配。
struct TreeRowRenderContext<'a> {
    side: usize,
    node_map: &'a TreeNodeMap,
    pane_entity: Entity<FilesPane>,
    drag_source: TreeDragSource,
    show_hidden: bool,
    theme: ThemeTokens,
}

/// 树行绘制快照关联可见节点、完整 endpoint 和事件目标，避免行状态参数错配。
///
/// @author kongweiguang
struct TreeEntryRowView<'a> {
    source_directory: FileEndpoint,
    source_generation: u64,
    selected_drag_entries: Arc<Vec<FileEntry>>,
    included_descendants: usize,
    side: usize,
    tree_entry: &'a TreeEntry,
    focused: TreeEntryState,
    endpoint: FileEndpoint,
    entry: FileEntry,
    node_map: &'a TreeNodeMap,
    pane_entity: Entity<FilesPane>,
    show_hidden: bool,
    theme: ThemeTokens,
}

/// 将当前 endpoint 根目录投影为 GPUI Kit 虚拟树；直接使用 Base Tree 时显式让内部列表与外层树填满浏览区域。
/// 这样保留虚拟列表自身的滚动布局，不依赖组件兼容层隐式补充的尺寸样式。
/// 浏览实底与普通列表保持一致，不让壁纸穿透目录文字。
/// 加载和未读取状态不复用紧凑列表专属的主机信任操作，避免在树模式暴露不匹配的入口。
///
/// @author kongweiguang
pub(super) fn render_tree_content(
    pane: &mut FilesPane,
    side: usize,
    loading: bool,
    listing: Option<&DirectoryListing>,
    show_hidden: bool,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> impl IntoElement {
    if loading {
        return browser::render_directory_state(
            IconName::LoaderCircle,
            locale::text("正在读取目录", "Reading directory"),
            locale::text(
                "读取完成后显示目录树。",
                "The tree appears when loading completes.",
            ),
            true,
            None,
            theme,
        )
        .into_any_element();
    }
    let Some(listing) = listing else {
        return browser::render_directory_state(
            IconName::FolderOpen,
            locale::text("目录尚未读取", "Directory not loaded"),
            locale::text(
                "刷新即可重新读取当前路径。",
                "Refresh to read the current path.",
            ),
            false,
            None,
            theme,
        )
        .into_any_element();
    };

    let root_entries = pane.panels[side].tree.root_entries(show_hidden);
    if root_entries.is_empty() {
        return browser::render_empty_directory(side, !listing.entries.is_empty(), theme, cx)
            .into_any_element();
    }

    pane.ensure_tree_widget(side, cx);
    let Some(widget) = pane.panels[side].tree_widget.as_ref() else {
        return div().into_any_element();
    };
    let tree_state = widget.state.clone();
    let tree_scroll_handle = tree_state.read(cx).scroll_handle().clone();
    let key_state = tree_state.clone();
    let node_map = widget.node_map.clone();
    let key_node_map = node_map.clone();
    let key_show_hidden = show_hidden;
    let source_directory = pane.panels[side].endpoint.clone();
    let source_generation = pane.panels[side].generation;
    let transfer_plan = pane.panels[side].tree.transfer_selection_plan(show_hidden);
    let selected_drag_entries = Arc::new(transfer_plan.roots);
    let included_descendants = transfer_plan.included_descendants;
    let pane_entity = cx.entity().clone();
    let tree = Tree::new(&tree_state)
        .list_style(StyleRefinement::default().flex_grow_1().size_full())
        .item(move |_, entry, focused, _, _| {
            render_tree_row(
                entry,
                focused,
                TreeRowRenderContext {
                    side,
                    node_map: &node_map,
                    pane_entity: pane_entity.clone(),
                    drag_source: TreeDragSource {
                        directory: source_directory.clone(),
                        generation: source_generation,
                        selected_entries: Arc::clone(&selected_drag_entries),
                        included_descendants,
                    },
                    show_hidden,
                    theme,
                },
            )
        })
        .relative()
        .size_full();

    div()
        .id(format!("files-tree-content-{side}"))
        .flex_1()
        .w_full()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(theme.panel)
        .on_key_down(cx.listener(move |pane, event: &KeyDownEvent, window, cx| {
            let key = event.keystroke.key.to_ascii_lowercase();
            let modifiers = event.keystroke.modifiers;
            if key == "delete" {
                if !pane.panels[side]
                    .tree
                    .selected_entries(key_show_hidden)
                    .is_empty()
                {
                    pane.activate_side(side, cx);
                    pane.delete_selected(cx);
                    cx.stop_propagation();
                }
                return;
            }
            if key == "f2" {
                let selected = pane.panels[side].tree.selected_entries(key_show_hidden);
                if selected.len() == 1 {
                    pane.select_tree_entry(
                        side,
                        selected[0].endpoint.clone(),
                        SelectionGesture::Replace,
                        cx,
                    );
                    pane.activate_side(side, cx);
                    pane.prepare_rename(window, cx);
                    cx.stop_propagation();
                }
                return;
            }
            if key == "a" && modifiers.secondary() {
                pane.toggle_select_all(side, cx);
                cx.stop_propagation();
                return;
            }

            let focused_id = key_state
                .read(cx)
                .selected_item()
                .map(|item| item.id.to_string());
            let Some(focused_id) = focused_id else {
                return;
            };
            let node = key_node_map.nodes.get(&focused_id).cloned();
            match key.as_str() {
                "space" | " " => {
                    let Some(TreeNode::Entry(endpoint)) = node else {
                        return;
                    };
                    let gesture = if modifiers.shift && modifiers.secondary() {
                        SelectionGesture::AddRange
                    } else if modifiers.shift {
                        SelectionGesture::Extend
                    } else {
                        SelectionGesture::Toggle
                    };
                    pane.select_tree_entry(side, endpoint, gesture, cx);
                    cx.stop_propagation();
                }
                "enter" => match node {
                    Some(TreeNode::Entry(endpoint)) => {
                        if let Some(entry) = pane.panels[side].tree.entry(&endpoint)
                            && entry.kind == FileEntryKind::File
                        {
                            pane.open_entry(side, entry, cx);
                            cx.stop_propagation();
                        }
                    }
                    Some(TreeNode::Retry(endpoint)) => {
                        pane.load_tree_directory(side, endpoint, cx);
                        cx.stop_propagation();
                    }
                    Some(TreeNode::Placeholder) | None => {}
                },
                _ => {}
            }
        }))
        .child(tree)
        .vertical_scrollbar(&tree_scroll_handle)
        .into_any_element()
}

/// 行绘制保持目录代次与拖拽选择的同源快照；双击与 Enter 仍复用同一 endpoint 打开逻辑。
///
/// @author kongweiguang
fn render_tree_row(
    tree_entry: &TreeEntry,
    focused: TreeEntryState,
    context: TreeRowRenderContext<'_>,
) -> AnyElement {
    let TreeRowRenderContext {
        side,
        node_map,
        pane_entity,
        drag_source,
        show_hidden,
        theme,
    } = context;
    let TreeDragSource {
        directory: source_directory,
        generation: source_generation,
        selected_entries: selected_drag_entries,
        included_descendants,
    } = drag_source;
    let id = tree_entry.item().id.to_string();
    let Some(node) = node_map.nodes.get(&id).cloned() else {
        return div().into_any_element();
    };
    match node {
        TreeNode::Entry(endpoint) => {
            let Some(entry) = node_map.entries.get(&endpoint).cloned() else {
                return div().into_any_element();
            };
            render_entry_row(TreeEntryRowView {
                source_directory,
                source_generation,
                selected_drag_entries,
                included_descendants,
                side,
                tree_entry,
                focused,
                endpoint,
                entry,
                node_map,
                pane_entity,
                show_hidden,
                theme,
            })
        }
        TreeNode::Retry(endpoint) => {
            render_retry_row(side, tree_entry, endpoint, pane_entity, theme)
        }
        TreeNode::Placeholder => render_placeholder_row(tree_entry, theme),
    }
}

/// 树节点按完整 endpoint 展示；非目录在展开位保留同宽空白，让文件类型图标只出现一次且名称对齐。
/// 默认行、选中和悬停均使用成对主题令牌，保持目录文本的稳定实底。
/// 复选框整个命中区由 Kit Base 处理单次切换并隔离树行事件，指示器显式沿用文件面板主题。
/// 展开箭头保留 Kit 的按下展开与聚焦，只阻断点击冒泡，避免同次操作再触发行的批量选择。
///
/// @author kongweiguang
fn render_entry_row(view: TreeEntryRowView<'_>) -> AnyElement {
    let TreeEntryRowView {
        source_directory,
        source_generation,
        selected_drag_entries,
        included_descendants,
        side,
        tree_entry,
        focused,
        endpoint,
        entry,
        node_map,
        pane_entity,
        show_hidden,
        theme,
    } = view;
    let context_directory = source_directory.clone();
    let context_generation = source_generation;
    let selected = node_map.selected.contains(&endpoint);
    let context_selected_entries = Arc::new(
        node_map
            .visible_endpoints
            .iter()
            .filter(|selected_endpoint| node_map.selected.contains(*selected_endpoint))
            .filter_map(|selected_endpoint| node_map.entries.get(selected_endpoint).cloned())
            .collect::<Vec<_>>(),
    );
    let context_multi_selection = selected && context_selected_entries.len() > 1;
    let can_change_permissions =
        matches!(entry.kind, FileEntryKind::File | FileEntryKind::Directory)
            && entry.permissions.is_some()
            && super::super::permissions::supports_posix_mode_endpoint(&entry.endpoint);
    let kind_icon = match entry.kind {
        FileEntryKind::Directory => IconName::FolderOpen,
        FileEntryKind::File => IconName::FileText,
        FileEntryKind::Symlink => IconName::ExternalLink,
        FileEntryKind::Other => IconName::File,
    };
    let disclosure = tree_entry.is_folder().then(|| {
        if tree_entry.is_expanded() {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        }
    });
    let mut disclosure_slot = div()
        .id(format!(
            "files-tree-disclosure-{side}-{}",
            tree_entry.item().id
        ))
        .w(px(14.))
        .h(px(14.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme.subtle);
    if let Some(disclosure) = disclosure {
        disclosure_slot = disclosure_slot
            .child(disclosure)
            .on_click(|_, _, cx| cx.stop_propagation());
    }
    let indent = px(8. + tree_entry.depth() as f32 * 14.);
    let click_entity = pane_entity.clone();
    let click_entry = entry.clone();
    let checkbox_entity = pane_entity.clone();
    let checkbox_endpoint = endpoint.clone();
    let checkbox_id = format!("files-tree-checkbox-{side}-{}", tree_entry.item().id);
    let checkbox = Checkbox::new(checkbox_id)
        .w(px(18.))
        .h(px(22.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .checked(selected)
        .accessibility_label(locale::text(
            format!("选择 {}", entry.name),
            format!("Select {}", entry.name),
        ))
        .tab_stop(false)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_change(move |_, _, window, app| {
            app.stop_propagation();
            let endpoint = checkbox_endpoint.clone();
            checkbox_entity.update(app, |pane, cx| {
                pane.toggle_tree_checkbox(side, endpoint, window, cx);
            });
        })
        .child(
            CheckboxIndicator::new()
                .checked(selected)
                .size(px(12.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .border_1()
                .border_color(theme.subtle)
                .bg(theme.panel)
                .text_size(theme.text_xs)
                .text_color(theme.accent)
                .styles(|styles| {
                    styles.checked(|style| style.border_color(theme.accent).bg(theme.selected))
                })
                .when(selected, |indicator| indicator.child(IconName::Check)),
        );
    let mut row = div()
        .id(format!("files-tree-row-{side}-{}", tree_entry.item().id))
        .w_full()
        .h(px(30.))
        .flex_shrink_0()
        .pl(indent)
        .pr_2()
        .flex()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(theme.border)
        .rounded_sm()
        .bg(if selected {
            theme.selected
        } else if focused.is_selected() {
            theme.hover
        } else {
            theme.panel
        })
        .hover(|style| style.bg(theme.hover))
        .when(focused.is_selected(), |style| {
            style.border_color(theme.accent.opacity(0.65))
        })
        .cursor_pointer()
        .child(checkbox)
        .child(disclosure_slot)
        .child(
            div()
                .w(px(16.))
                .flex_shrink_0()
                .text_color(theme.accent)
                .child(kind_icon),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(theme.text_body)
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .text_color(if selected {
                    theme.selected_foreground
                } else {
                    theme.foreground
                })
                .child(entry.name.clone()),
        );

    row = row.on_click(move |event: &ClickEvent, _, app| {
        let modifiers = event.modifiers();
        let gesture = if modifiers.shift && modifiers.secondary() {
            SelectionGesture::AddRange
        } else if modifiers.shift {
            SelectionGesture::Extend
        } else if modifiers.secondary() {
            SelectionGesture::Toggle
        } else {
            SelectionGesture::Replace
        };
        let double_click = event.click_count() > 1;
        click_entity.update(app, |pane, cx| {
            pane.select_tree_entry(side, click_entry.endpoint.clone(), gesture, cx);
            if double_click {
                pane.open_entry(side, click_entry.clone(), cx);
            }
        });
    });
    if matches!(entry.kind, FileEntryKind::File | FileEntryKind::Directory) {
        let drag_payload = if selected {
            FileTransferDrag::new(
                side,
                source_directory,
                source_generation,
                Arc::clone(&selected_drag_entries),
                included_descendants,
            )
        } else {
            FileTransferDrag::single(side, source_directory, source_generation, entry.clone())
        };
        row = row.on_drag(drag_payload, move |payload, _, _, app| {
            let count = payload.selected_count();
            app.new(move |_| FileDragPreview::new(count, theme))
        });
    }

    let menu_entity = pane_entity.clone();
    let menu_entry = entry.clone();
    let context_selection = Arc::clone(&context_selected_entries);
    let context_show_hidden = show_hidden;
    row.context_menu(move |mut menu, _, _| {
        menu = crate::ui_popup::context_menu(menu, theme, None);
        if context_multi_selection {
            let transferable_count = context_selection
                .iter()
                .filter(|entry| {
                    matches!(
                        entry.kind,
                        FileEntryKind::File | FileEntryKind::Directory | FileEntryKind::Symlink
                    ) && !matches!(&entry.endpoint, FileEndpoint::Local { .. })
                })
                .count();
            let selected_count = context_selection.len();
            let action_entity = menu_entity.clone();
            let action_selection = Arc::clone(&context_selection);
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text(
                    if transferable_count == selected_count {
                        format!("下载选中 {selected_count} 项")
                    } else {
                        format!("下载可传输 {transferable_count} 项")
                    },
                    if transferable_count == selected_count {
                        format!("Download {selected_count} selected items")
                    } else {
                        format!("Download {transferable_count} transferable items")
                    },
                ))
                .icon(Icon::new(AssetIconName::Download))
                .disabled(transferable_count == 0)
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if select_tree_selection(
                            pane,
                            side,
                            &action_selection,
                            context_generation,
                            &action_directory,
                            context_show_hidden,
                            cx,
                        ) {
                            pane.prompt_download(cx);
                        }
                    });
                }),
            );
            menu = menu.item(PopupMenuItem::separator());

            let action_entity = menu_entity.clone();
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("刷新目录", "Refresh directory"))
                    .icon(IconName::RefreshCw)
                    .on_click(move |_, _, app| {
                        action_entity.update(app, |pane, cx| {
                            if pane.browser_action_snapshot_is_current(
                                side,
                                context_generation,
                                &action_directory,
                            ) {
                                pane.activate_side(side, cx);
                                pane.refresh_panel(side, cx);
                            }
                        });
                    }),
            );

            let action_entity = menu_entity.clone();
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(if context_show_hidden {
                    locale::text("隐藏隐藏文件", "Hide hidden files")
                } else {
                    locale::text("显示隐藏文件", "Show hidden files")
                })
                .icon(if context_show_hidden {
                    IconName::EyeOff
                } else {
                    IconName::Eye
                })
                .checked(context_show_hidden)
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if pane.browser_action_snapshot_is_current(
                            side,
                            context_generation,
                            &action_directory,
                        ) {
                            pane.activate_side(side, cx);
                            pane.toggle_hidden_files(cx);
                        }
                    });
                }),
            );
            menu = menu.item(PopupMenuItem::separator());

            let action_entity = menu_entity.clone();
            let action_selection = Arc::clone(&context_selection);
            let action_directory = context_directory.clone();
            return menu.item(
                PopupMenuItem::new(locale::text(
                    format!("删除选中 {selected_count} 项"),
                    format!("Delete {selected_count} selected items"),
                ))
                .icon(Icon::new(AssetIconName::Trash))
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if select_tree_selection(
                            pane,
                            side,
                            &action_selection,
                            context_generation,
                            &action_directory,
                            context_show_hidden,
                            cx,
                        ) {
                            pane.delete_selected(cx);
                        }
                    });
                }),
            );
        }

        if menu_entry.kind == FileEntryKind::Directory {
            let action_entity = menu_entity.clone();
            let action_entry = menu_entry.clone();
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("打开", "Open"))
                    .icon(IconName::FolderOpen)
                    .on_click(move |_, _, app| {
                        action_entity.update(app, |pane, cx| {
                            if select_tree_row(
                                pane,
                                side,
                                &action_entry,
                                context_generation,
                                &action_directory,
                                cx,
                            ) {
                                pane.open_entry(side, action_entry.clone(), cx);
                            }
                        });
                    }),
            );
            let action_entity = menu_entity.clone();
            let action_entry = menu_entry.clone();
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("工作区打开", "Open in workspace"))
                    .icon(Icon::new(AssetIconName::FileSearch))
                    .on_click(move |_, _, app| {
                        action_entity.update(app, |pane, cx| {
                            if select_tree_row(
                                pane,
                                side,
                                &action_entry,
                                context_generation,
                                &action_directory,
                                cx,
                            ) {
                                pane.load_panel_at(side, action_entry.endpoint.clone(), cx);
                                pane.set_file_view_mode(
                                    super::super::view_mode::FilesViewMode::Workspace,
                                    cx,
                                );
                            }
                        });
                    }),
            );
        } else {
            let action_entity = menu_entity.clone();
            let action_entry = menu_entry.clone();
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("打开编辑器", "Open in editor"))
                    .icon(Icon::new(AssetIconName::FileSearch))
                    .disabled(menu_entry.kind != FileEntryKind::File)
                    .on_click(move |_, _, app| {
                        action_entity.update(app, |pane, cx| {
                            if select_tree_row(
                                pane,
                                side,
                                &action_entry,
                                context_generation,
                                &action_directory,
                                cx,
                            ) {
                                cx.emit(super::super::OpenFileRequested {
                                    endpoint: action_entry.endpoint.clone(),
                                });
                            }
                        });
                    }),
            );
        }

        let action_entity = menu_entity.clone();
        let action_entry = menu_entry.clone();
        let action_directory = context_directory.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text(
                if menu_entry.kind == FileEntryKind::Directory {
                    "下载文件夹"
                } else {
                    "下载"
                },
                if menu_entry.kind == FileEntryKind::Directory {
                    "Download folder"
                } else {
                    "Download"
                },
            ))
            .icon(Icon::new(AssetIconName::Download))
            .disabled(
                matches!(&menu_entry.endpoint, FileEndpoint::Local { .. })
                    || !matches!(
                        menu_entry.kind,
                        FileEntryKind::File | FileEntryKind::Directory | FileEntryKind::Symlink
                    ),
            )
            .on_click(move |_, _, app| {
                action_entity.update(app, |pane, cx| {
                    if select_tree_row(
                        pane,
                        side,
                        &action_entry,
                        context_generation,
                        &action_directory,
                        cx,
                    ) {
                        pane.prompt_download(cx);
                    }
                });
            }),
        );

        if matches!(&menu_entry.endpoint, FileEndpoint::Remote { .. }) {
            let action_entity = menu_entity.clone();
            let action_entry = menu_entry.clone();
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("下载到剪贴板", "Download to clipboard"))
                    .icon(Icon::new(AssetIconName::Download))
                    .disabled(!matches!(
                        menu_entry.kind,
                        FileEntryKind::File | FileEntryKind::Directory | FileEntryKind::Symlink
                    ))
                    .on_click(move |_, _, app| {
                        action_entity.update(app, |pane, cx| {
                            if select_tree_row(
                                pane,
                                side,
                                &action_entry,
                                context_generation,
                                &action_directory,
                                cx,
                            ) {
                                pane.request_remote_files_to_system_clipboard(
                                    side,
                                    Some(action_entry.clone()),
                                    cx,
                                );
                            }
                        });
                    }),
            );
        }

        if menu_entry.kind == FileEntryKind::Directory
            && !matches!(&menu_entry.endpoint, FileEndpoint::Local { .. })
        {
            let action_entity = menu_entity.clone();
            let action_entry = menu_entry.clone();
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text(
                    "上传文件到此目录",
                    "Upload files into this folder",
                ))
                .icon(Icon::new(AssetIconName::Upload))
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if select_tree_row(
                            pane,
                            side,
                            &action_entry,
                            context_generation,
                            &action_directory,
                            cx,
                        ) {
                            pane.prompt_upload_files_to(action_entry.endpoint.clone(), cx);
                        }
                    });
                }),
            );

            let action_entity = menu_entity.clone();
            let action_entry = menu_entry.clone();
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text(
                    "上传文件夹到此目录",
                    "Upload folders into this folder",
                ))
                .icon(Icon::new(AssetIconName::FolderPlus))
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if select_tree_row(
                            pane,
                            side,
                            &action_entry,
                            context_generation,
                            &action_directory,
                            cx,
                        ) {
                            pane.prompt_upload_directories_to(action_entry.endpoint.clone(), cx);
                        }
                    });
                }),
            );

            if matches!(&menu_entry.endpoint, FileEndpoint::Remote { .. }) {
                let action_entity = menu_entity.clone();
                let action_entry = menu_entry.clone();
                let action_directory = context_directory.clone();
                menu = menu.item(
                    PopupMenuItem::new(locale::text("粘贴到此目录", "Paste into this folder"))
                        .icon(Icon::new(AssetIconName::ClipboardPaste))
                        .on_click(move |_, _, app| {
                            action_entity.update(app, |pane, cx| {
                                if select_tree_row(
                                    pane,
                                    side,
                                    &action_entry,
                                    context_generation,
                                    &action_directory,
                                    cx,
                                ) {
                                    pane.paste_remote_clipboard(action_entry.endpoint.clone(), cx);
                                }
                            });
                        }),
                );
            }
        }

        menu = menu.item(PopupMenuItem::separator());
        if matches!(&menu_entry.endpoint, FileEndpoint::Remote { .. }) {
            let action_entity = menu_entity.clone();
            let action_entry = menu_entry.clone();
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("复制项目", "Copy item"))
                    .icon(Icon::new(AssetIconName::Copy))
                    .disabled(!matches!(
                        menu_entry.kind,
                        FileEntryKind::File | FileEntryKind::Directory | FileEntryKind::Symlink
                    ))
                    .on_click(move |_, _, app| {
                        action_entity.update(app, |pane, cx| {
                            if select_tree_row(
                                pane,
                                side,
                                &action_entry,
                                context_generation,
                                &action_directory,
                                cx,
                            ) {
                                pane.copy_to_remote_clipboard(side, Some(action_entry.clone()), cx);
                            }
                        });
                    }),
            );
        }

        let action_entity = menu_entity.clone();
        let action_entry = menu_entry.clone();
        let action_directory = context_directory.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text("复制路径", "Copy path"))
                .icon(Icon::new(AssetIconName::Copy))
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if select_tree_row(
                            pane,
                            side,
                            &action_entry,
                            context_generation,
                            &action_directory,
                            cx,
                        ) {
                            pane.copy_path_to_clipboard(&action_entry.endpoint, cx);
                        }
                    });
                }),
        );

        let action_entity = menu_entity.clone();
        let action_entry = menu_entry.clone();
        let action_directory = context_directory.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text("重命名", "Rename"))
                .icon(Icon::new(AssetIconName::Pencil))
                .on_click(move |_, window, app| {
                    action_entity.update(app, |pane, cx| {
                        if select_tree_row(
                            pane,
                            side,
                            &action_entry,
                            context_generation,
                            &action_directory,
                            cx,
                        ) {
                            pane.prepare_rename(window, cx);
                        }
                    });
                }),
        );

        if can_change_permissions {
            let action_entity = menu_entity.clone();
            let action_entry = menu_entry.clone();
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("修改权限", "Change permissions"))
                    .icon(Icon::new(AssetIconName::Pencil))
                    .on_click(move |_, window, app| {
                        action_entity.update(app, |pane, cx| {
                            if select_tree_row(
                                pane,
                                side,
                                &action_entry,
                                context_generation,
                                &action_directory,
                                cx,
                            ) {
                                pane.prepare_permissions(side, window, cx);
                            }
                        });
                    }),
            );
        }

        menu = menu.item(PopupMenuItem::separator());

        let action_entity = menu_entity.clone();
        let action_entry = menu_entry.clone();
        let action_directory = context_directory.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text(
                if menu_entry.kind == FileEntryKind::Directory {
                    "删除目录"
                } else {
                    "删除"
                },
                if menu_entry.kind == FileEntryKind::Directory {
                    "Delete folder"
                } else {
                    "Delete"
                },
            ))
            .icon(Icon::new(AssetIconName::Trash))
            .on_click(move |_, _, app| {
                action_entity.update(app, |pane, cx| {
                    if select_tree_row(
                        pane,
                        side,
                        &action_entry,
                        context_generation,
                        &action_directory,
                        cx,
                    ) {
                        pane.delete_selected(cx);
                    }
                });
            }),
        );

        menu
    })
    .into_any_element()
}

/// 树行右键操作冻结完整 endpoint；对已选行保留多选，其它行右键时切换为单选。
///
/// @author kongweiguang
fn select_tree_row(
    pane: &mut FilesPane,
    side: usize,
    entry: &FileEntry,
    generation: u64,
    directory: &FileEndpoint,
    cx: &mut Context<FilesPane>,
) -> bool {
    if !pane.browser_action_snapshot_is_current(side, generation, directory) {
        return false;
    }
    pane.activate_side(side, cx);
    if !pane.panels[side].tree.is_selected(&entry.endpoint) {
        pane.select_tree_entry(side, entry.endpoint.clone(), SelectionGesture::Replace, cx);
    }
    true
}

/// 批量树菜单要求捕获的 endpoint 集合和顺序仍与当前选择一致，防止对新选择执行旧命令。
///
/// @author kongweiguang
fn select_tree_selection(
    pane: &mut FilesPane,
    side: usize,
    expected: &[FileEntry],
    generation: u64,
    directory: &FileEndpoint,
    show_hidden: bool,
    cx: &mut Context<FilesPane>,
) -> bool {
    if !pane.browser_action_snapshot_is_current(side, generation, directory) {
        return false;
    }
    let current = pane.panels[side].tree.selected_entries(show_hidden);
    if current.len() != expected.len()
        || current
            .iter()
            .zip(expected)
            .any(|(left, right)| left.endpoint != right.endpoint)
    {
        pane.status = Some(
            locale::text(
                "右键菜单中的选择已变化，请重新打开菜单。",
                "The context menu selection changed. Open the menu again.",
            )
            .to_string(),
        );
        cx.notify();
        return false;
    }
    pane.activate_side(side, cx);
    true
}

/// 失败目录行只提供明确重试；加载、未加载与空目录行保持不可交互。
///
/// @author kongweiguang
fn render_retry_row(
    side: usize,
    tree_entry: &TreeEntry,
    endpoint: FileEndpoint,
    pane_entity: Entity<FilesPane>,
    theme: ThemeTokens,
) -> AnyElement {
    let label = tree_entry.item().label.clone();
    let request_entity = pane_entity;
    div()
        .id(format!("files-tree-retry-{side}-{}", tree_entry.item().id))
        .w_full()
        .h(px(30.))
        .flex_shrink_0()
        .pl(px(8. + tree_entry.depth() as f32 * 14.))
        .pr_2()
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .hover(|style| style.bg(theme.hover))
        .text_size(theme.text_sm)
        .text_color(theme.warning)
        .child(IconName::RefreshCw)
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .text_ellipsis()
                .child(label),
        )
        .on_click(move |_, _, app| {
            let endpoint = endpoint.clone();
            request_entity.update(app, |pane, cx| {
                pane.load_tree_directory(side, endpoint, cx);
            });
        })
        .into_any_element()
}

/// 占位行保持树深度和状态文案，但不伪装成可操作文件。
///
/// @author kongweiguang
fn render_placeholder_row(tree_entry: &TreeEntry, theme: ThemeTokens) -> AnyElement {
    let label = tree_entry.item().label.clone();
    let lower = label.to_string().to_lowercase();
    let icon = if lower.contains("loading") || lower.contains("正在读取") {
        IconName::LoaderCircle
    } else if lower.contains("empty") || lower.contains("空目录") {
        IconName::FolderOpen
    } else {
        IconName::ChevronRight
    };
    div()
        .w_full()
        .h(px(28.))
        .flex_shrink_0()
        .pl(px(8. + tree_entry.depth() as f32 * 14.))
        .pr_2()
        .flex()
        .items_center()
        .gap_2()
        .text_size(theme.text_sm)
        .text_color(theme.subtle)
        .child(icon)
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .text_ellipsis()
                .child(label),
        )
        .into_any_element()
}

#[cfg(test)]
#[path = "../../../../tests/unit/files_tree_context_menu_regressions.rs"]
mod context_menu_regression_tests;
