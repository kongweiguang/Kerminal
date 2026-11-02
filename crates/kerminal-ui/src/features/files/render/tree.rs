//! 文件工作区的 Kit 虚拟树绘制；行操作统一回到 FilesPane 既有用例。
//!
//! @author kongweiguang

use std::sync::Arc;

use gpui_kit::{
    AnyElement, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement, KeyDownEvent,
    MouseButton, StatefulInteractiveElement as _, StyleRefinement,
    base::{Checkbox, CheckboxIndicator, Tree, TreeEntry, TreeEntryState},
    component::{IconName, menu::ContextMenuExt as _, scroll::ScrollableElement as _},
    div,
    prelude::*,
    px,
};
use kerminal_domain::files::{DirectoryListing, FileEndpoint, FileEntry, FileEntryKind};

use crate::{locale, theme::ThemeTokens};

#[cfg(test)]
use super::super::context_menu::{select_tree_row, select_tree_selection};
use super::super::{
    FilesPane,
    context_menu::{FileMenuSnapshot, FileMenuTarget, build_file_context_menu},
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
    all_selected_entries: Arc<Vec<FileEntry>>,
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
    all_selected_drag_entries: Arc<Vec<FileEntry>>,
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
/// 已有同目录清单时继续呈现树；首次读取只保留框架，刷新图标承担唯一的加载反馈。
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
    if loading && listing.is_none() {
        return div().flex_1().into_any_element();
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
    let all_selected_drag_entries = Arc::new(pane.panels[side].tree.selected_entries(show_hidden));
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
                        all_selected_entries: Arc::clone(&all_selected_drag_entries),
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
        all_selected_entries: all_selected_drag_entries,
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
                all_selected_drag_entries,
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
/// 默认行、选中和悬停均使用成对主题令牌；右键菜单与列表共用精简后的命令范围。
/// 复选框整个命中区由 Kit Base 处理单次切换并隔离树行事件，指示器显式沿用文件面板主题。
/// 展开箭头保留 Kit 的按下展开与聚焦，只阻断点击冒泡，避免同次操作再触发行的批量选择。
///
/// @author kongweiguang
fn render_entry_row(view: TreeEntryRowView<'_>) -> AnyElement {
    let TreeEntryRowView {
        source_directory,
        source_generation,
        selected_drag_entries,
        all_selected_drag_entries,
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
    let selected = node_map.selected.contains(&endpoint);
    let context_selected_entries = Arc::new(
        node_map
            .visible_endpoints
            .iter()
            .filter(|selected_endpoint| node_map.selected.contains(*selected_endpoint))
            .filter_map(|selected_endpoint| node_map.entries.get(selected_endpoint).cloned())
            .collect::<Vec<_>>(),
    );
    let context_snapshot = FileMenuSnapshot {
        side,
        generation: source_generation,
        directory: source_directory.clone(),
        show_hidden,
        target: FileMenuTarget::Tree {
            entry: entry.clone(),
            selected: context_selected_entries,
            row_selected: selected,
        },
    };
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
    let row_debug_selector = format!(
        "files-tree-entry-{side}-{}",
        browser::endpoint_path(&entry.endpoint)
    );
    let mut row = div()
        .id(format!("files-tree-row-{side}-{}", tree_entry.item().id))
        .debug_selector(move || row_debug_selector.clone())
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
                pane_entity.entity_id().as_u64(),
                side,
                source_directory,
                source_generation,
                Arc::clone(&selected_drag_entries),
                included_descendants,
            )
            .with_tree_selection(Arc::clone(&all_selected_drag_entries))
        } else {
            FileTransferDrag::single(
                pane_entity.entity_id().as_u64(),
                side,
                source_directory,
                source_generation,
                entry.clone(),
            )
        };
        row = row.on_drag(drag_payload, move |payload, cursor_offset, _, app| {
            let count = payload.selected_count();
            app.new(move |_| FileDragPreview::new(count, cursor_offset, theme))
        });
        #[cfg(target_os = "windows")]
        {
            let drag_entity = pane_entity.clone();
            row = row.external_drag_payload(move |drag: &FileTransferDrag, _, app| {
                drag_entity.update(app, |pane, cx| pane.external_drag_payload(drag, cx))
            });
        }
    }

    let menu_entity = pane_entity.clone();
    row.context_menu(move |menu, _, app| {
        build_file_context_menu(
            menu,
            theme,
            menu_entity.clone(),
            context_snapshot.clone(),
            app,
        )
    })
    .into_any_element()
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
