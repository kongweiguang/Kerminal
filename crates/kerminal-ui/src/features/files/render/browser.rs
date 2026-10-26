//! 文件双栏面板的路径栏、工具栏和目录列表。该模块只投影现有 FileService 状态。
//!
//! @author kongweiguang

use chrono::{DateTime, Datelike, Utc};
use gpui_kit::{
    AnyElement, ClickEvent, Context, DispatchPhase, ExternalPaths, InteractiveElement as _,
    IntoElement, KeyDownEvent, MouseDownEvent, Pixels, ScrollWheelEvent,
    StatefulInteractiveElement as _, Window,
    base::v_virtual_list,
    canvas,
    component::{
        Disableable as _, IconName,
        button::{Button, ButtonVariants as _},
        input::Input,
        menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem},
        scroll::ScrollableElement as _,
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px, size,
};
use kerminal_application::{AppError, AppResult};
use kerminal_domain::files::{ConflictPolicy, FileEndpoint, FileEntry, FileEntryKind, RemotePath};
use std::{cmp::Ordering, path::PathBuf, rc::Rc, sync::Arc};

use crate::{locale, theme::ThemeTokens};
use gpui_kit::assets::IconName as AssetIconName;

use super::super::drag_drop::{
    DropDirectorySnapshot, FileDragPreview, FileTransferDrag, external_drop_directory_ready,
    external_paths_drop_is_allowed, internal_drop_is_allowed,
};
use super::super::{FILE_LIST_ROW_HEIGHT_PX, FileSort, FileSortColumn};
use super::{FilesPane, icon_button, icon_button_without_tooltip};

/// 单个目录面板的动作视图快照，宽窄布局共用同一份选择、目标和测量后的工具栏状态。
///
/// @author kongweiguang
struct FileActionsView<'a> {
    side: usize,
    active: bool,
    copy_target_available: bool,
    transfer_submission_busy: bool,
    loading: bool,
    conflict_policy: ConflictPolicy,
    delete_confirmation: bool,
    endpoint: &'a FileEndpoint,
    selected: Option<&'a FileEntry>,
    selected_count: usize,
    has_remote_selection: bool,
    visible_count: usize,
    all_visible_selected: bool,
    compact_layout: bool,
    show_hidden_files: bool,
    view_mode: super::super::view_mode::FilesViewMode,
    can_go_back: bool,
    can_go_forward: bool,
    show_compact_toolbar_actions: bool,
}

/// 一行文件展示所需的列表位置、选择状态和主题，事件仍回到 FilesPane 用例。
///
/// @author kongweiguang
struct FileEntryRowView {
    source_directory: FileEndpoint,
    source_generation: u64,
    selected_drag_entries: Arc<Vec<FileEntry>>,
    side: usize,
    index: usize,
    entry: FileEntry,
    selected: bool,
    keyboard_active: bool,
    ordered_names: Arc<Vec<String>>,
    columns: FileTableColumns,
    theme: ThemeTokens,
}

/// 表头与目录行共用实际面板宽度档位，窄栏先保留名称，权限只占用足够宽的空间。
///
/// @author kongweiguang
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileTableColumns {
    size: bool,
    modified: bool,
    permissions: bool,
}

/// 目录面板尺寸同时决定表格列和紧凑工具栏的可见操作，避免各自测量而产生瞬时溢出。
///
/// @author kongweiguang
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileBrowserLayout {
    columns: FileTableColumns,
    show_compact_toolbar_actions: bool,
}

/// 路径栏控件只接收本次渲染所需快照，避免把导航状态和主题拆成脆弱的位置参数。
///
/// @author kongweiguang
struct FilePathBarView<'a> {
    endpoint: &'a FileEndpoint,
    path_input: &'a gpui_kit::Entity<gpui_kit::component::input::InputState>,
    side: usize,
    show_terminal_follow: bool,
    bookmarks: Vec<(String, String, FileEndpoint)>,
    current_path_bookmarkable: bool,
    current_path_is_bookmarked: bool,
    can_go_back: bool,
    can_go_forward: bool,
    follow_terminal_directory: bool,
    theme: ThemeTokens,
}

/// 路径和动作固定分成两排；跟随控制只出现在可绑定当前 SSH pane 的紧凑右栏。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileBrowserToolbarRow {
    Path,
    Actions,
}

/// 双排工具栏的唯一布局投影，渲染器与回归测试共用顺序和紧凑 SSH 能力判断。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileBrowserToolbarProjection {
    rows: [FileBrowserToolbarRow; 2],
    show_terminal_follow: bool,
}

impl FileBrowserToolbarProjection {
    /// 固定路径先于动作，避免窄栏将地址与操作挤入同一行；跟随仅适用于紧凑 SSH 目标。
    ///
    /// @author kongweiguang
    fn for_endpoint(compact_layout: bool, endpoint: &FileEndpoint) -> Self {
        Self {
            rows: [FileBrowserToolbarRow::Path, FileBrowserToolbarRow::Actions],
            show_terminal_follow: compact_layout && matches!(endpoint, FileEndpoint::Remote { .. }),
        }
    }
}

/// 一个可排序列的轻量绘制快照，列样式与当前排序状态一起传递，不改变排序语义。
///
/// @author kongweiguang
struct SortableHeadingView {
    side: usize,
    column: FileSortColumn,
    label: gpui_kit::SharedString,
    sort: FileSort,
    width: Option<Pixels>,
    right_aligned: bool,
    theme: ThemeTokens,
}

impl FileTableColumns {
    /// 权限列在右栏可达到的宽度内显现；窄栏仍先省略权限和次级元数据。
    ///
    /// @author kongweiguang
    fn for_width(width: Pixels) -> Self {
        Self {
            size: width >= px(320.),
            modified: width >= px(440.),
            permissions: width >= px(600.),
        }
    }
}

impl FileBrowserLayout {
    /// 窄栏先保证路径与列表可读；仅在完整一行操作确实有空间时直显低频设置项。
    ///
    /// @author kongweiguang
    fn for_width(width: Pixels) -> Self {
        Self {
            columns: FileTableColumns::for_width(width),
            show_compact_toolbar_actions: width >= px(640.),
        }
    }
}

/// 视图按钮将三态入口收在稳定分组中，窄栏动作行换行时仍保持完整点击面积。
///
/// @author kongweiguang
fn render_file_view_controls(
    side: usize,
    selected: super::super::view_mode::FilesViewMode,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> impl IntoElement {
    let modes = [
        (
            super::super::view_mode::FilesViewMode::List,
            AssetIconName::List,
            locale::text("列表视图", "List view"),
        ),
        (
            super::super::view_mode::FilesViewMode::Tree,
            AssetIconName::ListTree,
            locale::text("树形视图", "Tree view"),
        ),
        (
            super::super::view_mode::FilesViewMode::Workspace,
            AssetIconName::LayoutDashboard,
            locale::text("编辑器工作区", "Editor workspace"),
        ),
    ];
    div()
        .id(format!("files-view-modes-{side}"))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_1()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .children(modes.into_iter().map(|(target, icon, label)| {
            let active = selected == target;
            Button::new(format!("files-view-{side}-{target:?}"))
                .compact()
                .w(px(32.))
                .h(px(32.))
                .flex_shrink_0()
                .when(active, Button::primary)
                .when(!active, Button::ghost)
                .icon(icon)
                .tooltip(label.clone())
                .accessibility_label(label)
                .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                    pane.activate_side(side, cx);
                    pane.set_file_view_mode(target, cx);
                }))
        }))
}

/// 路径和操作统一为两排；紧凑模式仅显示当前终端目标，工具栏和列按当前面板实际 bounds 分档。
/// 键盘焦点留在列表 owner；文件预览回流后在 viewport prepaint 中消费一次可见性意图。
///
/// @author kongweiguang
pub(super) fn render_browser(
    pane: &mut FilesPane,
    side: usize,
    window: &mut Window,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> impl IntoElement {
    let path_input = pane.ensure_path_input(side, window, cx);
    let shared_host_picker = side == 1 && pane.host_picker.enabled && !pane.compact_layout;
    let host_picker_header = shared_host_picker
        .then(|| super::host_picker::render_selector_header(pane, theme, window, cx));
    let layout_state = window.use_keyed_state(
        format!("files-browser-layout-{}-{side}", cx.entity_id().as_u64()),
        cx,
        |_, _| FileBrowserLayout::for_width(px(0.)),
    );
    let layout = *layout_state.read(cx);
    let columns = layout.columns;
    let column_measurement = canvas(
        move |bounds, _, cx| {
            let measured = FileBrowserLayout::for_width(bounds.size.width);
            if *layout_state.read(cx) == measured {
                return;
            }
            cx.defer(move |cx| {
                layout_state.update(cx, |layout, cx| {
                    if *layout != measured {
                        *layout = measured;
                        cx.notify();
                    }
                });
            });
        },
        |_, (), _, _| {},
    )
    .absolute()
    .inset_0();
    let selected_entries = pane.selected_entries(side);
    let selected_count = selected_entries.len();
    let has_remote_selection = selected_entries
        .iter()
        .any(|entry| !matches!(&entry.endpoint, FileEndpoint::Local { .. }));
    let panel = &pane.panels[side];
    let endpoint = panel.endpoint.clone();
    let toolbar_projection =
        FileBrowserToolbarProjection::for_endpoint(pane.compact_layout, &endpoint);
    let active = pane.active_side == side;
    let locked = side == 0 && pane.locked_left_host_id.is_some();
    let selected = panel.selected.clone();
    let panel_generation = panel.generation;
    let directory_ready = external_drop_directory_ready(
        &endpoint,
        panel.listing.as_ref().map(|listing| &listing.directory),
        panel.loading,
    );
    let transfer_submission_busy = pane.transfer_submission_busy;
    let delete_confirmation = pane.delete_confirmation.as_ref().is_some_and(|confirmed| {
        confirmed.len() == selected_entries.len()
            && confirmed
                .iter()
                .zip(&selected_entries)
                .all(|(left, right)| left == &right.endpoint)
    });
    let selected_drag_entries = if pane.view_mode == super::super::view_mode::FilesViewMode::Tree {
        Arc::new(Vec::new())
    } else {
        Arc::new(selected_entries)
    };
    let conflict_policy = pane.queue.conflict_policy;
    let loading = panel.loading;
    let listing = panel.listing.clone();
    let show_hidden_files = pane.show_hidden_files;
    let mut all_entries = listing
        .as_ref()
        .map(|listing| listing.entries.clone())
        .unwrap_or_default();
    all_entries.retain(|entry| show_hidden_files || !is_hidden_entry(entry));
    let all_count = all_entries.len();
    let file_count = all_entries
        .iter()
        .filter(|entry| entry.kind == FileEntryKind::File)
        .count();
    let directory_count = all_entries
        .iter()
        .filter(|entry| entry.kind == FileEntryKind::Directory)
        .count();
    let mut entries = all_entries
        .into_iter()
        .filter(|entry| panel.entry_filter.matches(entry.kind))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| compare_entries(left, right, panel.sort));
    let ordered_names = Arc::new(
        entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<Vec<_>>(),
    );
    let list_focus_handle = panel
        .list_focus_handle
        .clone()
        .expect("FilesPane::initialize creates one focus owner per browser panel");
    let keyboard_active_name = list_focus_handle
        .is_focused(window)
        .then(|| {
            panel
                .keyboard_focus
                .active_name(&endpoint, ordered_names.as_slice())
                .map(str::to_owned)
        })
        .flatten();
    let tree_mode = pane.view_mode == super::super::view_mode::FilesViewMode::Tree;
    let tree_visible = if tree_mode {
        panel.tree.visible_endpoints(show_hidden_files)
    } else {
        Vec::new()
    };
    let all_visible_selected = if tree_mode {
        !tree_visible.is_empty()
            && tree_visible
                .iter()
                .all(|endpoint| panel.tree.is_selected(endpoint))
    } else {
        !ordered_names.is_empty()
            && ordered_names
                .iter()
                .all(|name| panel.selection.contains(name))
    };
    let visible_count = if tree_mode {
        tree_visible.len()
    } else {
        entries.len()
    };
    let bookmark_key = super::super::bookmark_target(&endpoint);
    let current_path_bookmarkable = bookmark_key.is_some();
    let current_path_is_bookmarked = bookmark_key.as_ref().is_some_and(|(target, path)| {
        pane.bookmarks
            .iter()
            .any(|bookmark| &bookmark.target == target && &bookmark.path == path)
    });
    let path_bookmarks = pane
        .bookmarks
        .iter()
        .filter_map(|bookmark| {
            let destination = super::super::bookmark_endpoint(&endpoint, bookmark)?;
            let label = bookmark
                .label
                .clone()
                .unwrap_or_else(|| bookmark.path.clone());
            Some((label, bookmark.path.clone(), destination))
        })
        .collect::<Vec<_>>();

    let endpoint_heading: AnyElement = if pane.compact_layout {
        div().h(px(0.)).into_any_element()
    } else if let Some(header) = host_picker_header {
        header
    } else {
        render_endpoint_heading(
            &endpoint,
            side,
            locked,
            active,
            pane.target_names.display(&endpoint),
            pane.target_names.endpoint_detail(&endpoint),
            theme,
        )
        .into_any_element()
    };
    let mut panel_view = div()
        .id(format!("files-browser-{side}"))
        .flex_1()
        .min_w(px(0.))
        .h_full()
        .relative()
        .flex()
        .flex_col()
        .overflow_hidden()
        .rounded_md()
        .border_1()
        .border_color(if active {
            theme.accent.opacity(0.55)
        } else {
            theme.border
        })
        .bg(theme.panel);
    let drop_endpoint_for_validation = endpoint.clone();
    let drop_endpoint_for_external = endpoint.clone();
    let drop_endpoint_for_internal = endpoint.clone();
    let drop_endpoint_for_external_feedback = endpoint.clone();
    let drop_endpoint_for_internal_feedback = endpoint.clone();
    let source_side_for_drop = 1 - side;
    let source_panel = &pane.panels[source_side_for_drop];
    let source_endpoint_for_drop = source_panel.endpoint.clone();
    let source_generation_for_drop = source_panel.generation;
    let source_loading_for_drop = source_panel.loading;
    let source_listing_directory_for_drop = source_panel
        .listing
        .as_ref()
        .map(|listing| listing.directory.clone());
    let source_endpoint_for_validation = source_endpoint_for_drop.clone();
    let source_listing_for_validation = source_listing_directory_for_drop.clone();
    let source_endpoint_for_feedback = source_endpoint_for_drop;
    let source_listing_for_feedback = source_listing_directory_for_drop;
    panel_view = panel_view
        .can_drop(move |payload, _, _| {
            if let Some(paths) = payload.downcast_ref::<ExternalPaths>() {
                external_paths_drop_is_allowed(
                    &drop_endpoint_for_validation,
                    directory_ready,
                    transfer_submission_busy,
                    !paths.paths().is_empty(),
                )
            } else if let Some(drag) = payload.downcast_ref::<FileTransferDrag>() {
                internal_drop_is_allowed(
                    drag,
                    source_side_for_drop,
                    DropDirectorySnapshot {
                        endpoint: &source_endpoint_for_validation,
                        generation: source_generation_for_drop,
                        loading: source_loading_for_drop,
                        listing_directory: source_listing_for_validation.as_ref(),
                    },
                    side,
                    &drop_endpoint_for_validation,
                    directory_ready,
                    transfer_submission_busy,
                )
            } else {
                false
            }
        })
        .drag_over::<ExternalPaths>(move |style, paths, _, _| {
            if external_paths_drop_is_allowed(
                &drop_endpoint_for_external_feedback,
                directory_ready,
                transfer_submission_busy,
                !paths.paths().is_empty(),
            ) {
                style.border_2().border_color(theme.accent)
            } else {
                style
            }
        })
        .drag_over::<FileTransferDrag>(move |style, drag, _, _| {
            if internal_drop_is_allowed(
                drag,
                source_side_for_drop,
                DropDirectorySnapshot {
                    endpoint: &source_endpoint_for_feedback,
                    generation: source_generation_for_drop,
                    loading: source_loading_for_drop,
                    listing_directory: source_listing_for_feedback.as_ref(),
                },
                side,
                &drop_endpoint_for_internal_feedback,
                directory_ready,
                transfer_submission_busy,
            ) {
                style.border_2().border_color(theme.accent)
            } else {
                style
            }
        })
        .on_drop(cx.listener(move |pane, paths: &ExternalPaths, _, cx| {
            pane.handle_external_paths_drop(
                side,
                drop_endpoint_for_external.clone(),
                panel_generation,
                paths.paths().to_vec(),
                cx,
            );
        }))
        .on_drop(cx.listener(move |pane, drag: &FileTransferDrag, _, cx| {
            pane.handle_internal_file_drop(
                drag.clone(),
                side,
                drop_endpoint_for_internal.clone(),
                panel_generation,
                cx,
            );
        }))
        .child(column_measurement)
        .child(endpoint_heading);
    let path_bar = render_path_bar(
        FilePathBarView {
            endpoint: &endpoint,
            path_input: &path_input,
            side,
            show_terminal_follow: toolbar_projection.show_terminal_follow,
            bookmarks: path_bookmarks,
            current_path_bookmarkable,
            current_path_is_bookmarked,
            can_go_back: panel.navigation_history.can_go_back(),
            can_go_forward: panel.navigation_history.can_go_forward(),
            follow_terminal_directory: pane.follow_terminal_directory,
            theme,
        },
        cx,
    )
    .into_any_element();
    let actions_bar = render_file_actions(
        FileActionsView {
            side,
            active,
            copy_target_available: super::super::can_transfer_to_other_side(
                pane.host_picker.enabled,
                pane.host_picker.right_target_unselected,
                side,
            ),
            transfer_submission_busy: pane.transfer_submission_busy,
            conflict_policy,
            delete_confirmation,
            endpoint: &endpoint,
            selected: selected.as_ref(),
            selected_count,
            has_remote_selection,
            visible_count,
            all_visible_selected,
            compact_layout: pane.compact_layout,
            show_hidden_files,
            view_mode: pane.view_mode,
            loading,
            can_go_back: panel.navigation_history.can_go_back(),
            can_go_forward: panel.navigation_history.can_go_forward(),
            show_compact_toolbar_actions: layout.show_compact_toolbar_actions,
        },
        theme,
        cx,
    );
    let mut path_bar = Some(path_bar);
    let mut actions_bar = Some(actions_bar);
    for row in toolbar_projection.rows {
        panel_view = match row {
            FileBrowserToolbarRow::Path => panel_view.child(
                path_bar
                    .take()
                    .expect("path toolbar row is projected exactly once"),
            ),
            FileBrowserToolbarRow::Actions => panel_view.child(
                actions_bar
                    .take()
                    .expect("actions toolbar row is projected exactly once"),
            ),
        };
    }
    panel_view = panel_view.when(!pane.compact_layout && !tree_mode, |panel_view| {
        panel_view.child(render_file_kind_filter(
            side,
            panel.entry_filter,
            all_count,
            file_count,
            directory_count,
            theme,
            cx,
        ))
    });
    panel_view = panel_view.child(render_table_heading(
        side,
        panel.sort,
        theme,
        None,
        if tree_mode {
            FileTableColumns::for_width(px(0.))
        } else {
            columns
        },
        !tree_mode,
        cx,
    ));

    if tree_mode {
        return panel_view.child(super::tree::render_tree_content(
            pane,
            side,
            loading,
            listing.as_ref(),
            show_hidden_files,
            theme,
            cx,
        ));
    }

    let keyboard_names = Arc::clone(&ordered_names);
    let keyboard_directory = endpoint.clone();
    let keyboard_generation = panel_generation;
    let mut list = div()
        .id(format!("files-browser-list-{side}"))
        .flex_1()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .overflow_hidden()
        .tab_group()
        .track_focus(&list_focus_handle)
        .tab_stop(true)
        .role(gpui_kit::Role::List)
        .aria_label(locale::text("文件列表", "File list"))
        .aria_keyshortcuts(concat!(
            "ArrowUp ArrowDown Shift+ArrowUp Shift+ArrowDown ",
            "PageUp PageDown Shift+PageUp Shift+PageDown ",
            "Home End Enter Space Control+A Meta+A",
        ))
        .on_key_down(cx.listener(move |pane, event: &KeyDownEvent, _, cx| {
            let key = event.keystroke.key.to_ascii_lowercase();
            if matches!(
                key.as_str(),
                "up" | "down" | "pageup" | "pagedown" | "home" | "end"
            ) {
                pane.move_list_keyboard_focus(
                    side,
                    keyboard_generation,
                    &keyboard_directory,
                    keyboard_names.as_slice(),
                    event,
                    cx,
                );
                cx.stop_propagation();
            } else if key == "enter" {
                pane.panels[side].keyboard_scroll_intent.cancel();
                if let Some(entry) = pane.keyboard_active_entry(
                    side,
                    keyboard_generation,
                    &keyboard_directory,
                    keyboard_names.as_slice(),
                ) {
                    pane.open_entry(side, entry, cx);
                }
                cx.stop_propagation();
            } else if matches!(key.as_str(), "space" | " ") {
                pane.panels[side].keyboard_scroll_intent.cancel();
                if let Some(entry) = pane.keyboard_active_entry(
                    side,
                    keyboard_generation,
                    &keyboard_directory,
                    keyboard_names.as_slice(),
                ) {
                    let modifiers = event.keystroke.modifiers;
                    let gesture = if modifiers.shift && modifiers.secondary() {
                        super::super::selection::SelectionGesture::AddRange
                    } else if modifiers.shift {
                        super::super::selection::SelectionGesture::Extend
                    } else {
                        super::super::selection::SelectionGesture::Toggle
                    };
                    pane.select_entry(side, entry, gesture, keyboard_names.as_slice(), cx);
                }
                cx.stop_propagation();
            } else if key == "a" && event.keystroke.modifiers.secondary() {
                pane.panels[side].keyboard_scroll_intent.cancel();
                let snapshot_is_current = pane.browser_action_snapshot_is_current(
                    side,
                    keyboard_generation,
                    &keyboard_directory,
                ) && pane.panels.get(side).is_some_and(|panel| {
                    !panel.loading
                        && panel
                            .listing
                            .as_ref()
                            .is_some_and(|listing| listing.directory == keyboard_directory)
                });
                if snapshot_is_current {
                    pane.toggle_select_all(side, cx);
                }
                cx.stop_propagation();
            }
        }))
        .bg(theme.panel);

    // The row viewport can shrink again when an asynchronous preview appears. This absolute
    // prepaint child is deliberately first so the virtual list consumes its one-shot Nearest
    // request using the final bounds in the same frame; captured pointer input cancels it.
    if let Some(request) = panel
        .keyboard_scroll_intent
        .pending
        .clone()
        .filter(|request| !request.consumed.get())
    {
        let target_index = ordered_names
            .iter()
            .position(|name| name == &request.target_name);
        let listing_is_current = !panel.loading
            && panel
                .listing
                .as_ref()
                .is_some_and(|listing| listing.directory == request.directory);
        let scroll_snapshot = super::super::KeyboardScrollSnapshot {
            panel_generation,
            directory: endpoint.clone(),
            listing_is_current,
            active_name: keyboard_active_name.clone(),
            selected_endpoint: selected.as_ref().map(|entry| entry.endpoint.clone()),
            preview_generation: panel.preview_generation,
            preview_pending: panel.preview_cancellation.is_some(),
        };
        let readiness = panel
            .keyboard_scroll_intent
            .readiness_for(request.revision, &scroll_snapshot);
        let mut scroll_intent = panel.keyboard_scroll_intent.clone();
        let scroll_intent_for_events = scroll_intent.clone();
        let list_focus_handle = list_focus_handle.clone();
        let list_scroll = panel.list_scroll.clone();
        let scroll_revision = request.revision;
        list = list.child(
            canvas(
                move |_, window, _app| {
                    let has_list_focus = list_focus_handle.is_focused(window);
                    let index = target_index;
                    let should_scroll = has_list_focus
                        && readiness == super::super::KeyboardScrollReadiness::Ready
                        && index.is_some()
                        && scroll_intent.consume_if_ready(scroll_revision, &scroll_snapshot);
                    if should_scroll {
                        list_scroll.scroll_to_item(
                            index.expect("a ready keyboard scroll has a visible target"),
                            gpui_kit::ScrollStrategy::Nearest,
                        );
                    }

                    if !has_list_focus
                        || index.is_none()
                        || readiness == super::super::KeyboardScrollReadiness::Stale
                        || should_scroll
                    {
                        scroll_intent.cancel_if_revision(scroll_revision);
                    }
                },
                move |bounds, _, window, _| {
                    let scroll_bounds = bounds;
                    let mut scroll_intent_for_wheel = scroll_intent_for_events.clone();
                    let mut scroll_intent_for_mouse_down = scroll_intent_for_events.clone();
                    window.on_mouse_event(move |event: &ScrollWheelEvent, phase, _, _| {
                        if phase == DispatchPhase::Capture
                            && scroll_bounds.contains(&event.position)
                        {
                            scroll_intent_for_wheel.cancel_if_revision(scroll_revision);
                        }
                    });
                    window.on_mouse_event(move |event: &MouseDownEvent, phase, _, _| {
                        if phase == DispatchPhase::Capture && bounds.contains(&event.position) {
                            scroll_intent_for_mouse_down.cancel_if_revision(scroll_revision);
                        }
                    });
                },
            )
            .absolute()
            .inset_0(),
        );
    }

    if loading {
        list = list.child(render_directory_state(
            IconName::LoaderCircle,
            locale::text("正在读取目录", "Reading directory"),
            locale::text(
                "文件与权限信息将显示在这里。",
                "Files and permissions will appear here.",
            ),
            true,
            theme,
        ));
    } else if listing.is_none() {
        list = list.child(render_directory_state(
            IconName::FolderOpen,
            locale::text("目录尚未读取", "Directory not loaded"),
            locale::text(
                "刷新即可重新读取当前路径。",
                "Refresh to read the current path.",
            ),
            false,
            theme,
        ));
    } else if entries.is_empty() {
        if panel.entry_filter != super::super::view_mode::FileEntryFilter::All {
            list = list.child(render_directory_state(
                IconName::Search,
                locale::text("没有匹配项目", "No matching items"),
                locale::text(
                    "切换“全部”可查看当前目录中的其它项目。",
                    "Switch to All to see other items in this directory.",
                ),
                false,
                theme,
            ));
        } else {
            let hidden_only = listing
                .as_ref()
                .is_some_and(|listing| !listing.entries.is_empty());
            list = list.child(render_empty_directory(side, hidden_only, theme, cx));
        }
    } else {
        let rows = Rc::new(entries);
        let row_sizes = Rc::new(vec![size(px(1.), px(FILE_LIST_ROW_HEIGHT_PX)); rows.len()]);
        let selected_rows = Arc::new(
            rows.iter()
                .map(|entry| panel.selection.contains(&entry.name))
                .collect::<Vec<_>>(),
        );
        let list_scroll = panel.list_scroll.clone();
        let pane_entity = cx.entity().clone();
        let source_directory = endpoint.clone();
        let row_names = Arc::clone(&ordered_names);
        let drag_entries = Arc::clone(&selected_drag_entries);
        let source_generation = panel_generation;
        let active_name_for_rows = keyboard_active_name.clone();
        let list_rows = v_virtual_list(
            pane_entity,
            format!("files-list-rows-{side}"),
            row_sizes,
            move |_, range, _, cx| {
                range
                    .map(|index| {
                        let entry = rows[index].clone();
                        let selected = selected_rows[index];
                        let keyboard_active =
                            active_name_for_rows.as_deref() == Some(entry.name.as_str());
                        render_entry(
                            FileEntryRowView {
                                side,
                                index,
                                entry,
                                selected,
                                keyboard_active,
                                ordered_names: Arc::clone(&row_names),
                                columns,
                                theme,
                                source_directory: source_directory.clone(),
                                source_generation,
                                selected_drag_entries: Arc::clone(&drag_entries),
                            },
                            cx,
                        )
                    })
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&list_scroll);
        list = list.child(list_rows).vertical_scrollbar(&list_scroll);
    }

    let background_entity = cx.entity().clone();
    let background_endpoint = endpoint.clone();
    let background_generation = panel_generation;
    let list = list.context_menu(move |mut menu, _, _| {
        let action_entity = background_entity.clone();
        let action_endpoint = background_endpoint.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text("全选当前可见项目", "Select all visible items"))
                .disabled(visible_count == 0)
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if pane.browser_action_snapshot_is_current(
                            side,
                            background_generation,
                            &action_endpoint,
                        ) {
                            pane.toggle_select_all(side, cx);
                        }
                    });
                }),
        );

        let action_entity = background_entity.clone();
        let action_endpoint = background_endpoint.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text("新建文件夹…", "New folder…")).on_click(
                move |_, window, app| {
                    action_entity.update(app, |pane, cx| {
                        if !pane.browser_action_snapshot_is_current(
                            side,
                            background_generation,
                            &action_endpoint,
                        ) {
                            return;
                        }
                        pane.activate_side(side, cx);
                        pane.prepare_create_directory(window, cx);
                    });
                },
            ),
        );

        if !matches!(&background_endpoint, FileEndpoint::Local { .. }) {
            let action_entity = background_entity.clone();
            let action_endpoint = background_endpoint.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("上传文件…", "Upload files…")).on_click(
                    move |_, _, app| {
                        action_entity.update(app, |pane, cx| {
                            if !pane.browser_action_snapshot_is_current(
                                side,
                                background_generation,
                                &action_endpoint,
                            ) {
                                return;
                            }
                            pane.activate_side(side, cx);
                            pane.prompt_upload(cx);
                        });
                    },
                ),
            );
        }

        let action_entity = background_entity.clone();
        let action_endpoint = background_endpoint.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text("刷新目录", "Refresh directory")).on_click(
                move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if pane.browser_action_snapshot_is_current(
                            side,
                            background_generation,
                            &action_endpoint,
                        ) {
                            pane.refresh_panel(side, cx);
                        }
                    });
                },
            ),
        );

        let action_entity = background_entity.clone();
        let action_endpoint = background_endpoint.clone();
        menu = menu.item(
            PopupMenuItem::new(if show_hidden_files {
                locale::text("隐藏点文件", "Hide dotfiles")
            } else {
                locale::text("显示点文件", "Show dotfiles")
            })
            .checked(show_hidden_files)
            .on_click(move |_, _, app| {
                action_entity.update(app, |pane, cx| {
                    if pane.browser_action_snapshot_is_current(
                        side,
                        background_generation,
                        &action_endpoint,
                    ) {
                        pane.toggle_hidden_files(cx);
                    }
                });
            }),
        );

        let action_entity = background_entity.clone();
        let action_endpoint = background_endpoint.clone();
        menu.item(
            PopupMenuItem::new(if tree_mode {
                locale::text("列表视图", "List view")
            } else {
                locale::text("树形视图", "Tree view")
            })
            .on_click(move |_, _, app| {
                action_entity.update(app, |pane, cx| {
                    if pane.browser_action_snapshot_is_current(
                        side,
                        background_generation,
                        &action_endpoint,
                    ) {
                        pane.toggle_tree_view(cx);
                    }
                });
            }),
        )
    });

    panel_view = panel_view.child(list);
    panel_view
}

/// 端点身份留在面板标题，当前路径另放路径栏；侧栏序号提供稳定唯一 ID，使完整身份可作无障碍名称。
///
/// @author kongweiguang
fn render_endpoint_heading(
    endpoint: &FileEndpoint,
    side: usize,
    locked: bool,
    active: bool,
    display: super::super::target_names::FileTargetDisplay,
    full_target: String,
    theme: ThemeTokens,
) -> impl IntoElement {
    let label = endpoint_short_label(endpoint, locked, &display);
    let accessibility_label = full_target.clone();
    let tooltip_label = full_target;
    div()
        .id(format!("files-endpoint-heading-{side}"))
        .w_full()
        .h(px(32.))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .aria_label(accessibility_label)
        .tooltip(move |window, cx| Tooltip::new(tooltip_label.clone()).build(window, cx))
        .child(div().w(px(6.)).h(px(6.)).rounded_full().bg(if active {
            theme.accent
        } else {
            theme.subtle
        }))
        .child(
            div()
                .flex_1()
                .text_size(theme.text_xs)
                .min_w(px(0.))
                .overflow_hidden()
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(if active {
                    theme.foreground
                } else {
                    theme.muted
                })
                .text_ellipsis()
                .child(label),
        )
}

/// 将 endpoint 身份浓缩到标签中，同时在锁定左端时显式保留约束提示。
///
/// @author kongweiguang
fn endpoint_short_label(
    endpoint: &FileEndpoint,
    locked: bool,
    display: &super::super::target_names::FileTargetDisplay,
) -> gpui_kit::SharedString {
    match endpoint {
        FileEndpoint::Local { .. } => locale::text("本机", "This device"),
        FileEndpoint::Remote { .. } if locked => locale::text(
            format!("{} · 已锁定", display.compact),
            format!("{} · locked", display.compact),
        ),
        FileEndpoint::Remote { .. } | FileEndpoint::Container { .. } => {
            display.compact.clone().into()
        }
    }
}

/// 路径栏把按钮与 Enter 入口送到同一导航校验；完整路径提示保证窄栏中的地址仍可核对。
///
/// @author kongweiguang
fn render_path_bar(view: FilePathBarView<'_>, cx: &mut Context<FilesPane>) -> impl IntoElement {
    let FilePathBarView {
        endpoint,
        path_input,
        side,
        show_terminal_follow,
        bookmarks,
        current_path_bookmarkable,
        current_path_is_bookmarked,
        can_go_back,
        can_go_forward,
        follow_terminal_directory,
        theme,
    } = view;
    let scheme = match endpoint {
        FileEndpoint::Local { .. } => "file://",
        FileEndpoint::Remote { .. } => "sftp://",
        FileEndpoint::Container { .. } => "container://",
    };
    let full_path = endpoint_path(endpoint);
    let path_field = div()
        .id(format!("files-path-{side}"))
        .min_w(px(0.))
        .h(px(32.))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .overflow_hidden()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .tooltip(move |window, cx| Tooltip::new(full_path.clone()).build(window, cx))
        .child(
            div()
                .flex_shrink_0()
                .text_size(theme.text_sm)
                .text_color(theme.accent)
                .child(scheme),
        )
        .child(
            div()
                .id(format!("files-path-keyboard-{side}"))
                .flex_1()
                .min_w(px(0.))
                .h_full()
                .flex()
                .items_center()
                .overflow_hidden()
                .on_key_down(cx.listener(move |pane, event: &KeyDownEvent, window, cx| {
                    match event.keystroke.key.to_ascii_lowercase().as_str() {
                        "enter" => {
                            pane.commit_path_edit(side, cx);
                            cx.stop_propagation();
                        }
                        "escape" => {
                            pane.cancel_path_edit(side, window, cx);
                            cx.stop_propagation();
                        }
                        _ => {}
                    }
                }))
                .child(
                    Input::new(path_input)
                        .w_full()
                        .h(px(30.))
                        .text_size(theme.text_sm),
                ),
        );

    let jump = Button::new(format!("files-path-jump-{side}"))
        .compact()
        .w(px(32.))
        .h(px(32.))
        .flex_shrink_0()
        .ghost()
        .icon(IconName::ArrowRight)
        .tooltip(locale::text(
            "跳转到输入路径（Enter）",
            "Go to entered path (Enter)",
        ))
        .accessibility_label(locale::text("跳转到输入路径", "Go to entered path"))
        .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| pane.commit_path_edit(side, cx)));
    let entity = cx.entity().clone();
    let bookmark_button = Button::new(format!("files-bookmark-menu-{side}"))
        .compact()
        .w(px(32.))
        .h(px(32.))
        .when(current_path_is_bookmarked, Button::primary)
        .when(!current_path_is_bookmarked, Button::ghost)
        .icon(AssetIconName::Bookmark)
        .tooltip(locale::text(
            "书签与收藏当前路径",
            "Bookmarks and bookmark this path",
        ))
        .accessibility_label(locale::text("打开路径书签", "Open path bookmarks"))
        .dropdown_menu(move |mut menu, _, _| {
            let action_entity = entity.clone();
            menu = menu.item(
                PopupMenuItem::new(if current_path_is_bookmarked {
                    locale::text("取消当前路径书签", "Remove bookmark for this path")
                } else {
                    locale::text("收藏当前路径", "Bookmark this path")
                })
                .checked(current_path_is_bookmarked)
                .disabled(!current_path_bookmarkable)
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| pane.toggle_bookmark(side, cx));
                }),
            );

            let action_entity = entity.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text(
                    "返回上一个目录",
                    "Go back to the previous directory",
                ))
                .disabled(!can_go_back)
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| pane.navigate_back(side, cx));
                }),
            );

            let action_entity = entity.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text(
                    "前进到下一个目录",
                    "Go forward to the next directory",
                ))
                .disabled(!can_go_forward)
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| pane.navigate_forward(side, cx));
                }),
            );
            if bookmarks.is_empty() {
                menu = menu.item(PopupMenuItem::separator()).item(
                    PopupMenuItem::label(locale::text(
                        "当前目标没有书签",
                        "No bookmarks for this target",
                    ))
                    .disabled(true),
                );
            } else {
                menu = menu.item(PopupMenuItem::separator());
                for (label, path, destination) in bookmarks.iter().cloned() {
                    let action_entity = entity.clone();
                    let display = if label == path {
                        label
                    } else {
                        format!("{label} · {path}")
                    };
                    menu = menu.item(PopupMenuItem::new(display).on_click(move |_, _, app| {
                        action_entity.update(app, |pane, cx| {
                            pane.open_endpoint_on_side(side, destination.clone(), cx);
                        });
                    }));
                }
            }
            menu
        });

    let follow = show_terminal_follow.then(|| {
        Button::new(format!("files-follow-terminal-{side}"))
            .compact()
            .w(px(32.))
            .h(px(32.))
            .when(follow_terminal_directory, Button::primary)
            .when(!follow_terminal_directory, Button::ghost)
            .icon(IconName::SquareTerminal)
            .tooltip(locale::text(
                "跟随当前 SSH 终端目录",
                "Follow the current SSH terminal directory",
            ))
            .accessibility_label(locale::text("跟随终端目录", "Follow terminal directory"))
            .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                pane.set_follow_terminal_directory(!follow_terminal_directory, cx);
            }))
    });

    div()
        .id(format!("files-path-bar-{side}"))
        .w_full()
        .h(px(48.))
        .flex_shrink_0()
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(path_field.flex_1())
        .child(bookmark_button)
        .child(jump)
        .when_some(follow, |bar, follow| bar.child(follow))
        .into_any_element()
}

/// 工具只调用既有文件用例；创建动作保留对象文字，冲突策略收进传输菜单，危险色仅在删除确认时出现。
///
/// @author kongweiguang
fn render_file_actions(
    view: FileActionsView<'_>,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    if view.compact_layout {
        return render_compact_file_actions(view, theme, cx);
    }
    let FileActionsView {
        side,
        active,
        copy_target_available,
        transfer_submission_busy,
        loading,
        conflict_policy,
        delete_confirmation,
        endpoint,
        selected,
        selected_count,
        has_remote_selection,
        visible_count,
        show_hidden_files,
        view_mode,
        can_go_back,
        can_go_forward,
        ..
    } = view;
    let has_selection = selected_count > 0;
    let has_file =
        selected_count == 1 && selected.is_some_and(|entry| entry.kind == FileEntryKind::File);
    let delete_label = if !has_selection {
        locale::text("请先选择要删除的项目", "Select an item before deleting")
    } else if delete_confirmation {
        locale::text("再次点击确认删除", "Click again to confirm deletion")
    } else {
        locale::text(
            "删除所选项目；再次点击确认",
            "Delete selected item; click again to confirm",
        )
    };
    let copy_label = if !copy_target_available {
        locale::text("请先选择右侧主机", "Select a host on the right first")
    } else if has_selection {
        if side == 0 {
            locale::text("复制到右侧", "Copy to right panel")
        } else {
            locale::text("复制到左侧", "Copy to left panel")
        }
    } else {
        locale::text("请先选择要复制的项目", "Select an item to copy")
    };
    let rename_label = if has_selection {
        locale::text("重命名所选项目", "Rename selected item")
    } else {
        locale::text("请先选择要重命名的项目", "Select an item before renaming")
    };
    let can_upload = !matches!(endpoint, FileEndpoint::Local { .. });
    let can_download = has_remote_selection;
    let back = icon_button(
        format!("files-back-{side}"),
        IconName::ArrowLeft,
        locale::text("返回上一个目录", "Go back to the previous directory"),
        !can_go_back,
    )
    .w(px(32.))
    .h(px(32.))
    .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| pane.navigate_back(side, cx)));
    let forward = icon_button(
        format!("files-forward-{side}"),
        IconName::ArrowRight,
        locale::text("前进到下一个目录", "Go forward to the next directory"),
        !can_go_forward,
    )
    .w(px(32.))
    .h(px(32.))
    .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| pane.navigate_forward(side, cx)));
    let parent = icon_button(
        format!("files-parent-{side}"),
        IconName::ArrowUp,
        locale::text("上级目录", "Parent directory"),
        super::super::parent_endpoint(endpoint).is_none(),
    )
    .w(px(32.))
    .h(px(32.))
    .flex_shrink_0()
    .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| pane.navigate_parent(side, cx)));
    let refresh = icon_button(
        format!("files-refresh-{side}"),
        IconName::RefreshCw,
        locale::text("刷新目录", "Refresh directory"),
        loading,
    )
    .w(px(32.))
    .h(px(32.))
    .loading(loading)
    .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| pane.refresh_panel(side, cx)));
    let mut actions = div()
        .id(format!("files-actions-{side}"))
        .w_full()
        .h(px(40.))
        .flex_shrink_0()
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .overflow_x_scroll()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(back)
        .child(forward)
        .child(parent)
        .child(refresh)
        .child(
            icon_button(
                format!("files-new-folder-{side}"),
                IconName::Plus,
                locale::text("新建文件夹", "New folder"),
                false,
            )
            .label(locale::text("新建文件夹", "New folder"))
            .w_auto()
            .h(px(32.))
            .on_click(cx.listener(move |pane, _: &ClickEvent, window, cx| {
                pane.activate_side(side, cx);
                pane.prepare_create_directory(window, cx);
            })),
        )
        .child(
            Button::new(format!("files-rename-{side}"))
                .compact()
                .flex_shrink_0()
                .h(px(32.))
                .ghost()
                .text_size(theme.text_xs)
                .label(locale::text("重命名", "Rename"))
                .tooltip(rename_label)
                .disabled(!has_selection)
                .on_click(cx.listener(move |pane, _: &ClickEvent, window, cx| {
                    pane.activate_side(side, cx);
                    pane.prepare_rename(window, cx);
                })),
        )
        .child(
            icon_button(
                format!("files-delete-{side}"),
                IconName::Delete,
                delete_label,
                !has_selection,
            )
            .w(px(32.))
            .h(px(32.))
            .when(delete_confirmation, Button::danger)
            .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                pane.activate_side(side, cx);
                pane.delete_selected(cx);
            })),
        )
        .child(div().w(px(1.)).h(px(16.)).flex_shrink_0().bg(theme.border))
        .child(
            icon_button(
                format!("files-copy-to-other-{side}"),
                if side == 0 {
                    IconName::ArrowRight
                } else {
                    IconName::ArrowLeft
                },
                if transfer_submission_busy {
                    locale::text("正在准备传输…", "Preparing transfer…")
                } else {
                    copy_label
                },
                !copy_target_available || !has_selection || transfer_submission_busy,
            )
            .w(px(32.))
            .h(px(32.))
            .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                pane.activate_side(side, cx);
                pane.transfer_selected(cx);
            })),
        );

    if can_upload {
        actions = actions.child(
            icon_button(
                "files-upload-current",
                IconName::ArrowUp,
                if transfer_submission_busy {
                    locale::text("正在准备传输…", "Preparing transfer…")
                } else {
                    locale::text(
                        "从本机选择文件传入此目录",
                        "Choose local files to upload here",
                    )
                },
                transfer_submission_busy,
            )
            .w(px(32.))
            .h(px(32.))
            .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                pane.activate_side(side, cx);
                pane.prompt_upload(cx);
            })),
        );
    }

    actions = actions.child(
        icon_button(
            format!("files-toggle-hidden-{side}"),
            if show_hidden_files {
                IconName::Eye
            } else {
                IconName::EyeOff
            },
            if show_hidden_files {
                locale::text("隐藏点文件", "Hide dotfiles")
            } else {
                locale::text("显示点文件", "Show dotfiles")
            },
            false,
        )
        .w(px(32.))
        .h(px(32.))
        .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| pane.toggle_hidden_files(cx))),
    );

    if can_upload {
        actions = actions.child(
            icon_button(
                format!("files-download-{side}"),
                IconName::ArrowDown,
                if transfer_submission_busy {
                    locale::text("正在准备传输…", "Preparing transfer…")
                } else {
                    locale::text(
                        "下载所选项目到本机",
                        "Download selected item to this device",
                    )
                },
                transfer_submission_busy || !can_download,
            )
            .w(px(32.))
            .h(px(32.))
            .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                pane.activate_side(side, cx);
                pane.prompt_download(cx);
            })),
        );
    }

    if active {
        actions = actions.child(render_conflict_policy_menu(
            side,
            conflict_policy,
            theme,
            cx,
        ));
    }

    let editor_tooltip = if has_file {
        locale::text("在编辑器中打开", "Open in editor")
    } else if has_selection {
        locale::text(
            "仅普通文件可在编辑器中打开",
            "Only regular files can open in the editor",
        )
    } else {
        locale::text(
            "选择文件后在编辑器中打开",
            "Select a file to open in the editor",
        )
    };
    let open_editor_button = icon_button_without_tooltip(
        format!("files-open-editor-{side}"),
        IconName::FileText,
        editor_tooltip.clone(),
        !has_file,
    )
    .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
        pane.activate_side(side, cx);
        if let Some(entry) = pane.panels[side].selected.as_ref()
            && entry.kind == FileEntryKind::File
        {
            cx.emit(super::super::OpenFileRequested {
                endpoint: entry.endpoint.clone(),
            });
        }
    }));
    actions
        .child(
            div()
                .id(format!("files-open-editor-tooltip-{side}"))
                .w(px(32.))
                .h(px(32.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .tooltip(move |window, cx| Tooltip::new(editor_tooltip.clone()).build(window, cx))
                .child(open_editor_button),
        )
        .child(div().flex_1().min_w(px(8.)))
        .child(
            div()
                .flex_shrink_0()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(if selected_count == 0 {
                    locale::text(
                        format!("{visible_count} 项"),
                        format!("{visible_count} items"),
                    )
                } else {
                    locale::text(
                        format!("已选 {selected_count} / {visible_count}"),
                        format!("{selected_count} / {visible_count} selected"),
                    )
                }),
        )
        .child(render_file_view_controls(side, view_mode, theme, cx))
        .into_any_element()
}

/// 紧凑栏把常用动作和低频动作放在同一组的响应式布局中，窄栏仍可从菜单找到全部操作。
/// 只有足够容纳整组操作时才直显点文件与带文字的新建目录，保留计数、三态视图和原异步状态机。
///
/// @author kongweiguang
fn render_compact_file_actions(
    view: FileActionsView<'_>,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    let FileActionsView {
        side,
        transfer_submission_busy,
        loading,
        conflict_policy,
        delete_confirmation,
        endpoint,
        selected,
        selected_count,
        has_remote_selection,
        visible_count,
        all_visible_selected,
        show_hidden_files,
        view_mode,
        can_go_back,
        can_go_forward,
        show_compact_toolbar_actions,
        ..
    } = view;
    let can_upload = !matches!(endpoint, FileEndpoint::Local { .. });
    let can_open_editor =
        selected_count == 1 && selected.is_some_and(|entry| entry.kind == FileEntryKind::File);
    let entity = cx.entity().clone();

    let more = Button::new(format!("files-more-actions-{side}"))
        .compact()
        .ghost()
        .w(px(32.))
        .h(px(32.))
        .flex_shrink_0()
        .icon(IconName::Ellipsis)
        .tooltip(locale::text("更多文件操作", "More file actions"))
        .accessibility_label(locale::text("更多文件操作", "More file actions"))
        .dropdown_menu(move |mut menu, _, _| {
            let action_entity = entity.clone();
            menu = menu.item(
                PopupMenuItem::new(if all_visible_selected {
                    locale::text("清除全选", "Clear selection")
                } else {
                    locale::text("全选当前可见项目", "Select all visible items")
                })
                .disabled(visible_count == 0)
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| pane.toggle_select_all(side, cx));
                }),
            );

            if !show_compact_toolbar_actions {
                let action_entity = entity.clone();
                menu = menu.item(
                    PopupMenuItem::new(locale::text("新建文件夹…", "New folder…")).on_click(
                        move |_, window, app| {
                            action_entity.update(app, |pane, cx| {
                                pane.activate_side(side, cx);
                                pane.prepare_create_directory(window, cx);
                            });
                        },
                    ),
                );

                let action_entity = entity.clone();
                menu = menu.item(
                    PopupMenuItem::new(if show_hidden_files {
                        locale::text("隐藏点文件", "Hide dotfiles")
                    } else {
                        locale::text("显示点文件", "Show dotfiles")
                    })
                    .checked(show_hidden_files)
                    .on_click(move |_, _, app| {
                        action_entity.update(app, |pane, cx| pane.toggle_hidden_files(cx));
                    }),
                );
            }

            let action_entity = entity.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text(
                    "返回上一个目录",
                    "Go back to the previous directory",
                ))
                .disabled(!can_go_back)
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| pane.navigate_back(side, cx));
                }),
            );

            let action_entity = entity.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text(
                    "前进到下一个目录",
                    "Go forward to the next directory",
                ))
                .disabled(!can_go_forward)
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| pane.navigate_forward(side, cx));
                }),
            );

            let action_entity = entity.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("重命名…", "Rename…"))
                    .disabled(selected_count != 1)
                    .on_click(move |_, window, app| {
                        action_entity.update(app, |pane, cx| {
                            pane.activate_side(side, cx);
                            pane.prepare_rename(window, cx);
                        });
                    }),
            );

            let action_entity = entity.clone();
            let delete_label = if delete_confirmation {
                locale::text("确认删除所选项目", "Confirm deleting selected items")
            } else {
                locale::text("删除所选项目…", "Delete selected items…")
            };
            menu = menu.item(
                PopupMenuItem::new(delete_label)
                    .disabled(selected_count == 0)
                    .on_click(move |_, _, app| {
                        action_entity.update(app, |pane, cx| {
                            pane.activate_side(side, cx);
                            pane.delete_selected(cx);
                        });
                    }),
            );

            let action_entity = entity.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("在编辑器中打开", "Open in editor"))
                    .disabled(!can_open_editor)
                    .on_click(move |_, _, app| {
                        action_entity.update(app, |pane, cx| {
                            pane.activate_side(side, cx);
                            let selected = pane.selected_entries(side);
                            if selected.len() == 1 && selected[0].kind == FileEntryKind::File {
                                cx.emit(super::super::OpenFileRequested {
                                    endpoint: selected[0].endpoint.clone(),
                                });
                            }
                        });
                    }),
            );

            menu = menu.item(PopupMenuItem::separator());
            menu = menu.item(
                PopupMenuItem::label(locale::text(
                    "后续任务的冲突策略",
                    "Conflict policy for new tasks",
                ))
                .disabled(true),
            );
            for (policy, label) in [
                (
                    ConflictPolicy::Rename,
                    locale::text("重命名同名项（默认）", "Rename conflicts (default)"),
                ),
                (
                    ConflictPolicy::Skip,
                    locale::text("跳过同名项", "Skip conflicts"),
                ),
                (
                    ConflictPolicy::Overwrite,
                    locale::text("覆盖文件；合并目录", "Overwrite files; merge folders"),
                ),
            ] {
                let action_entity = entity.clone();
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .checked(policy == conflict_policy)
                        .on_click(move |_, _, app| {
                            action_entity
                                .update(app, |pane, cx| pane.set_conflict_policy(policy, cx));
                        }),
                );
            }
            menu
        });

    let parent = icon_button(
        format!("files-parent-{side}"),
        IconName::ArrowUp,
        locale::text("上级目录", "Parent directory"),
        super::super::parent_endpoint(endpoint).is_none(),
    )
    .w(px(32.))
    .h(px(32.))
    .flex_shrink_0()
    .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| pane.navigate_parent(side, cx)));
    let refresh = icon_button(
        format!("files-refresh-{side}"),
        IconName::RefreshCw,
        locale::text("刷新目录", "Refresh directory"),
        loading,
    )
    .w(px(32.))
    .h(px(32.))
    .flex_shrink_0()
    .loading(loading)
    .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| pane.refresh_panel(side, cx)));

    let count_label = if selected_count == 0 {
        locale::text(
            format!("{visible_count} 项"),
            format!("{visible_count} items"),
        )
    } else {
        locale::text(
            format!("已选 {selected_count}/{visible_count}"),
            format!("{selected_count}/{visible_count} selected"),
        )
    };
    let count_tooltip = count_label.clone();

    let mut actions = div()
        .id(format!("files-actions-{side}"))
        .w_full()
        .min_h(px(40.))
        .flex_shrink_0()
        .px_3()
        .py_1()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(parent)
        .child(refresh);
    if show_compact_toolbar_actions {
        actions = actions
            .child(div().w(px(1.)).h(px(16.)).flex_shrink_0().bg(theme.border))
            .child(
                icon_button(
                    format!("files-toggle-hidden-{side}"),
                    if show_hidden_files {
                        IconName::Eye
                    } else {
                        IconName::EyeOff
                    },
                    if show_hidden_files {
                        locale::text("隐藏点文件", "Hide dotfiles")
                    } else {
                        locale::text("显示点文件", "Show dotfiles")
                    },
                    false,
                )
                .w(px(32.))
                .h(px(32.))
                .when(show_hidden_files, Button::primary)
                .when(!show_hidden_files, Button::ghost)
                .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| pane.toggle_hidden_files(cx))),
            )
            .child(div().w(px(1.)).h(px(16.)).flex_shrink_0().bg(theme.border));
    }
    if can_upload {
        actions = actions.child(
            icon_button(
                format!("files-upload-{side}"),
                IconName::ArrowUp,
                locale::text("从本机选择并上传", "Choose local files and upload"),
                transfer_submission_busy,
            )
            .w(px(32.))
            .h(px(32.))
            .flex_shrink_0()
            .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                pane.activate_side(side, cx);
                pane.prompt_upload(cx);
            })),
        );
    }
    if show_compact_toolbar_actions {
        actions = actions
            .child(
                icon_button(
                    format!("files-new-folder-{side}"),
                    IconName::Plus,
                    locale::text("在当前目录新建文件夹", "Create a folder in this directory"),
                    false,
                )
                .label(locale::text("新建文件夹", "New folder"))
                .w_auto()
                .h(px(32.))
                .on_click(cx.listener(move |pane, _: &ClickEvent, window, cx| {
                    pane.activate_side(side, cx);
                    pane.prepare_create_directory(window, cx);
                })),
            )
            .child(div().w(px(1.)).h(px(16.)).flex_shrink_0().bg(theme.border));
    }
    if can_upload {
        actions = actions.child(
            icon_button(
                format!("files-download-{side}"),
                IconName::ArrowDown,
                locale::text("下载所选项目", "Download selected items"),
                transfer_submission_busy || !has_remote_selection,
            )
            .w(px(32.))
            .h(px(32.))
            .flex_shrink_0()
            .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                pane.activate_side(side, cx);
                pane.prompt_download(cx);
            })),
        );
    }
    actions
        .child(
            div()
                .id(format!("files-toolbar-count-{side}"))
                .w(px(120.))
                .flex_shrink_0()
                .overflow_hidden()
                .text_ellipsis()
                .text_size(theme.text_xs)
                .text_color(theme.subtle)
                .aria_label(count_label.clone())
                .tooltip(move |window, cx| Tooltip::new(count_tooltip.clone()).build(window, cx))
                .child(count_label),
        )
        .child(render_file_view_controls(side, view_mode, theme, cx))
        .child(more)
        .into_any_element()
}

/// 将冲突策略放在可识别的传输偏好菜单中，所选值仍只影响之后加入队列的任务。
///
/// @author kongweiguang
fn render_conflict_policy_menu(
    side: usize,
    selected: ConflictPolicy,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> impl IntoElement {
    let policy_name = match selected {
        ConflictPolicy::Rename => locale::text("重命名", "Rename"),
        ConflictPolicy::Skip => locale::text("跳过", "Skip"),
        ConflictPolicy::Overwrite => locale::text("覆盖", "Overwrite"),
    };
    let trigger_label = locale::text(
        format!("传输 · {policy_name}"),
        format!("Transfer · {policy_name}"),
    );
    let entity = cx.entity().clone();

    Button::new(format!("files-transfer-preferences-{side}"))
        .compact()
        .ghost()
        .h(px(32.))
        .flex_shrink_0()
        .text_size(theme.text_xs)
        .icon(IconName::Settings2)
        .label(trigger_label.clone())
        .tooltip(locale::text(
            format!("冲突策略：{policy_name}；只影响之后加入的任务。"),
            format!("Conflict policy: {policy_name}; applies only to newly queued tasks."),
        ))
        .accessibility_label(trigger_label)
        .dropdown_menu(move |mut menu, _, _| {
            menu = menu
                .item(
                    PopupMenuItem::label(locale::text(
                        "以下设置只影响之后加入的任务",
                        "These settings apply only to newly queued tasks",
                    ))
                    .disabled(true),
                )
                .item(PopupMenuItem::separator());

            for (policy, label) in [
                (
                    ConflictPolicy::Rename,
                    locale::text("重命名同名项（默认）", "Rename conflicts (default)"),
                ),
                (
                    ConflictPolicy::Skip,
                    locale::text("跳过同名项", "Skip conflicts"),
                ),
                (
                    ConflictPolicy::Overwrite,
                    locale::text("覆盖文件；合并目录", "Overwrite files; merge folders"),
                ),
            ] {
                let entity = entity.clone();
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .checked(policy == selected)
                        .on_click(move |_, _, app| {
                            entity.update(app, |pane, cx| pane.set_conflict_policy(policy, cx));
                        }),
                );
            }
            menu
        })
}

/// 表头与目录行共享固定元数据宽度和显示档位；UTC 标示避免把服务时间误认为本地时间。
///
/// @author kongweiguang
fn render_table_heading(
    side: usize,
    sort: FileSort,
    theme: ThemeTokens,
    item_count: Option<usize>,
    columns: FileTableColumns,
    sortable: bool,
    cx: &mut Context<FilesPane>,
) -> impl IntoElement {
    let name_label: AnyElement = if sortable {
        sortable_heading(
            SortableHeadingView {
                side,
                column: FileSortColumn::Name,
                label: locale::text("名称", "Name"),
                sort,
                width: None,
                right_aligned: false,
                theme,
            },
            cx,
        )
        .into_any_element()
    } else {
        div()
            .flex()
            .items_center()
            .child(locale::text("名称", "Name"))
            .into_any_element()
    };
    let mut name_heading = div().flex().items_center().gap_1().child(name_label);
    name_heading = name_heading.flex_1().min_w(px(0.));
    if let Some(item_count) = item_count {
        name_heading = name_heading.child(div().text_color(theme.subtle).child(locale::text(
            format!("{item_count} 项"),
            format!("{item_count} items"),
        )));
    }
    let mut heading = div()
        .w_full()
        .h(px(26.))
        .flex_shrink_0()
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .text_size(theme.text_sm)
        .text_color(theme.subtle)
        .child(name_heading);
    if columns.permissions {
        heading = heading.child(
            div()
                .w(px(112.))
                .flex_shrink_0()
                .text_right()
                .child(locale::text("权限", "Permissions")),
        );
    }
    if columns.size {
        heading = heading.child(sortable_heading(
            SortableHeadingView {
                side,
                column: FileSortColumn::Size,
                label: locale::text("大小", "Size"),
                sort,
                width: Some(px(84.)),
                right_aligned: true,
                theme,
            },
            cx,
        ));
    }
    if columns.modified {
        heading = heading.child(sortable_heading(
            SortableHeadingView {
                side,
                column: FileSortColumn::Modified,
                label: locale::text("时间 · UTC", "Time · UTC"),
                sort,
                width: Some(px(136.)),
                right_aligned: true,
                theme,
            },
            cx,
        ));
    }
    heading
}

/// 类型计数按当前可见目录快照计算；过滤切换只改变列表投影，不触发额外目录请求。
///
/// @author kongweiguang
fn render_file_kind_filter(
    side: usize,
    selected: super::super::view_mode::FileEntryFilter,
    all_count: usize,
    file_count: usize,
    directory_count: usize,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> impl IntoElement {
    div()
        .id(format!("files-filter-{side}"))
        .w_full()
        .h(px(32.))
        .px_2()
        .flex()
        .items_center()
        .gap_1()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(render_file_kind_filter_button(
            side,
            super::super::view_mode::FileEntryFilter::All,
            selected,
            locale::text(format!("全部 {all_count}"), format!("All {all_count}")),
            locale::text(
                format!("显示全部 {all_count} 项"),
                format!("Show all {all_count} items"),
            ),
            theme,
            cx,
        ))
        .child(render_file_kind_filter_button(
            side,
            super::super::view_mode::FileEntryFilter::Files,
            selected,
            locale::text(format!("文件 {file_count}"), format!("Files {file_count}")),
            locale::text(
                format!("只显示 {file_count} 个文件"),
                format!("Show only {file_count} files"),
            ),
            theme,
            cx,
        ))
        .child(render_file_kind_filter_button(
            side,
            super::super::view_mode::FileEntryFilter::Directories,
            selected,
            locale::text(
                format!("目录 {directory_count}"),
                format!("Folders {directory_count}"),
            ),
            locale::text(
                format!("只显示 {directory_count} 个目录"),
                format!("Show only {directory_count} folders"),
            ),
            theme,
            cx,
        ))
}

/// 每个计数按钮保留键盘语义并沿用 Kit 的浅深主题按钮状态。
///
/// @author kongweiguang
fn render_file_kind_filter_button(
    side: usize,
    filter: super::super::view_mode::FileEntryFilter,
    selected: super::super::view_mode::FileEntryFilter,
    label: gpui_kit::SharedString,
    accessibility_label: gpui_kit::SharedString,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> Button {
    let active = filter == selected;
    Button::new(format!("files-filter-{side}-{filter:?}"))
        .compact()
        .text_size(theme.text_xs)
        .when(active, Button::primary)
        .when(!active, Button::ghost)
        .label(label)
        .accessibility_label(accessibility_label)
        .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
            pane.set_file_entry_filter(side, filter, cx)
        }))
}

/// 可排序表头提供鼠标与键盘入口，并用方向符号说明当前顺序。
///
/// @author kongweiguang
fn sortable_heading(view: SortableHeadingView, cx: &mut Context<FilesPane>) -> impl IntoElement {
    let SortableHeadingView {
        side,
        column,
        label,
        sort,
        width,
        right_aligned,
        theme,
    } = view;
    let label_text = label.to_string();
    let displayed = if sort.column == column {
        format!("{} {}", label_text, if sort.descending { "↓" } else { "↑" })
    } else {
        label_text.clone()
    };
    let accessible = locale::text(
        format!("按{label_text}排序"),
        format!("Sort by {label_text}"),
    );
    div()
        .id(format!("files-sort-{side}-{:?}", column))
        .when(width.is_none(), |this| this.flex_1().min_w(px(0.)))
        .when_some(width, |this, width| this.w(width).flex_shrink_0())
        .flex()
        .items_center()
        .when(right_aligned, |this| this.justify_end())
        .px_1()
        .rounded_sm()
        .cursor_pointer()
        .role(gpui_kit::Role::Button)
        .tab_index(0)
        .aria_label(accessible)
        .aria_keyshortcuts("Enter Space")
        .text_color(theme.subtle)
        .hover(|style| style.bg(theme.hover).text_color(theme.foreground))
        .on_key_down(cx.listener(move |pane, event: &KeyDownEvent, _, cx| {
            if matches!(
                event.keystroke.key.to_ascii_lowercase().as_str(),
                "enter" | "space" | " "
            ) {
                pane.toggle_sort(side, column, cx);
                cx.stop_propagation();
            }
        }))
        .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
            pane.toggle_sort(side, column, cx);
        }))
        .child(displayed)
}

/// 固定元数据列挤压文件名时保留完整名称提示；稳定行身份支持列表容器代理可访问焦点。
/// 行仍固定 44px 并由虚拟列表承载，不给回收行创建独立焦点句柄。
///
/// @author kongweiguang
fn render_entry(view: FileEntryRowView, cx: &mut Context<FilesPane>) -> AnyElement {
    let FileEntryRowView {
        side,
        index,
        entry,
        selected,
        keyboard_active,
        ordered_names,
        columns,
        theme,
        source_directory,
        source_generation,
        selected_drag_entries,
    } = view;
    let kind_icon = match entry.kind {
        FileEntryKind::Directory => IconName::FolderOpen,
        FileEntryKind::File => IconName::FileText,
        FileEntryKind::Symlink => IconName::ExternalLink,
        FileEntryKind::Other => IconName::File,
    };
    let accessible_kind = match entry.kind {
        FileEntryKind::Directory => locale::text("文件夹", "folder"),
        FileEntryKind::File => locale::text("文件", "file"),
        FileEntryKind::Symlink => locale::text("符号链接", "symbolic link"),
        FileEntryKind::Other => locale::text("特殊项目", "special item"),
    };
    let size = super::format_size(entry.size);
    let mode = if crate::features::files::permissions::supports_posix_mode_endpoint(&entry.endpoint)
    {
        entry
            .permissions
            .map(|mode| super::super::permissions::format_posix_mode(entry.kind, mode))
            .unwrap_or_else(|| "—".to_owned())
    } else {
        locale::text("Windows ACL", "Windows ACL").to_string()
    };
    let modified = format_file_modified(&entry);
    let modified_tooltip = format_file_modified_tooltip(&entry);
    let name = entry.name.clone();
    let name_tooltip = name.clone();
    let context_entry = entry.clone();
    let context_entity = cx.entity().clone();
    let context_names = Arc::clone(&ordered_names);
    let context_directory = source_directory.clone();
    let context_generation = source_generation;
    let accessible_name = format!("{}, {}", entry.name, accessible_kind);
    let click_entry = entry.clone();
    let double_click_entry = entry.clone();
    let click_names = Arc::clone(&ordered_names);
    let click_directory = source_directory.clone();
    let click_generation = source_generation;
    let mut row = div()
        .id(format!("files-row-{side}-{name}"))
        .w_full()
        .h(px(44.))
        .flex_shrink_0()
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(if keyboard_active {
            theme.accent
        } else {
            theme.border
        })
        .rounded_sm()
        .bg(if selected {
            theme.selected
        } else {
            theme.panel
        })
        .hover(|style| style.bg(theme.hover))
        .cursor_pointer()
        .role(gpui_kit::Role::ListItem)
        .aria_label(accessible_name)
        .aria_selected(selected)
        .aria_keyshortcuts("Enter Space Shift+Space Control+Space")
        .on_click(cx.listener(move |pane, event: &ClickEvent, window, cx| {
            if !pane.browser_action_snapshot_is_current(side, click_generation, &click_directory) {
                return;
            }
            let modifiers = event.modifiers();
            let gesture = if modifiers.shift && modifiers.secondary() {
                super::super::selection::SelectionGesture::AddRange
            } else if modifiers.shift {
                super::super::selection::SelectionGesture::Extend
            } else if modifiers.secondary() {
                super::super::selection::SelectionGesture::Toggle
            } else {
                super::super::selection::SelectionGesture::Replace
            };
            pane.select_entry(
                side,
                click_entry.clone(),
                gesture,
                click_names.as_slice(),
                cx,
            );
            pane.focus_browser_list(side, window, cx);
            if event.click_count() > 1 {
                pane.open_entry(side, double_click_entry.clone(), cx);
            }
        }))
        .child(
            div()
                .w(px(14.))
                .h(px(14.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .text_size(theme.text_xs)
                .text_color(if selected { theme.accent } else { theme.subtle })
                .child(if selected {
                    IconName::Check
                } else {
                    IconName::Square
                }),
        )
        .child(div().w(px(16.)).text_color(theme.accent).child(kind_icon))
        .child(
            div()
                .id(format!("files-entry-name-{name}"))
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .text_size(theme.text_body)
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .text_color(if selected {
                    theme.selected_foreground
                } else {
                    theme.foreground
                })
                .whitespace_nowrap()
                .text_ellipsis()
                .tooltip(move |window, cx| Tooltip::new(name_tooltip.clone()).build(window, cx))
                .child(name),
        );
    if keyboard_active {
        row = row.aria_active_descendant();
    }
    if matches!(entry.kind, FileEntryKind::File | FileEntryKind::Directory) {
        let drag_payload = if selected {
            FileTransferDrag::new(
                side,
                source_directory,
                source_generation,
                Arc::clone(&selected_drag_entries),
                0,
            )
        } else {
            FileTransferDrag::single(side, source_directory, source_generation, entry)
        };
        row = row.on_drag(drag_payload, move |payload, _, _, app| {
            let count = payload.selected_count();
            app.new(move |_| FileDragPreview::new(count, theme))
        });
    }
    if columns.permissions {
        row = row.child(
            div()
                .w(px(112.))
                .flex_shrink_0()
                .text_right()
                .text_size(theme.text_sm)
                .text_color(theme.subtle)
                .font_family("monospace")
                .child(mode),
        );
    }
    if columns.size {
        row = row.child(
            div()
                .w(px(84.))
                .flex_shrink_0()
                .text_right()
                .text_size(theme.text_sm)
                .text_color(theme.subtle)
                .child(size),
        );
    }
    if columns.modified {
        row = row.child(
            div()
                .id(format!("files-modified-{side}-{index}"))
                .w(px(136.))
                .flex_shrink_0()
                .overflow_hidden()
                .text_right()
                .text_size(theme.text_sm)
                .text_color(theme.subtle)
                .whitespace_nowrap()
                .text_ellipsis()
                .tooltip(move |window, cx| Tooltip::new(modified_tooltip.clone()).build(window, cx))
                .child(modified),
        );
    }
    row.context_menu(move |mut menu, _, _| {
        let action_entry = context_entry.clone();
        let action_entity = context_entity.clone();
        let action_directory = context_directory.clone();
        menu = menu.item(PopupMenuItem::new(locale::text("打开", "Open")).on_click(
            move |_, _, app| {
                action_entity.update(app, |pane, cx| {
                    if pane.browser_action_snapshot_is_current(
                        side,
                        context_generation,
                        &action_directory,
                    ) {
                        pane.open_entry(side, action_entry.clone(), cx);
                    }
                });
            },
        ));

        let action_entry = context_entry.clone();
        let action_entity = context_entity.clone();
        let action_names = Arc::clone(&context_names);
        let action_directory = context_directory.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text("重命名…", "Rename…")).on_click(
                move |_, window, app| {
                    action_entity.update(app, |pane, cx| {
                        if select_context_entry(
                            pane,
                            side,
                            action_entry.clone(),
                            &action_names,
                            context_generation,
                            &action_directory,
                            cx,
                        ) {
                            pane.prepare_rename(window, cx);
                        }
                    });
                },
            ),
        );

        let action_entry = context_entry.clone();
        let action_entity = context_entity.clone();
        let action_names = Arc::clone(&context_names);
        let action_directory = context_directory.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text("下载到本机…", "Download to this device…"))
                .disabled(matches!(
                    &context_entry.endpoint,
                    FileEndpoint::Local { .. }
                ))
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if select_context_entry(
                            pane,
                            side,
                            action_entry.clone(),
                            &action_names,
                            context_generation,
                            &action_directory,
                            cx,
                        ) {
                            pane.prompt_download(cx);
                        }
                    });
                }),
        );

        let action_entry = context_entry.clone();
        let action_entity = context_entity.clone();
        let action_names = Arc::clone(&context_names);
        let action_directory = context_directory.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text("删除…", "Delete…")).on_click(move |_, _, app| {
                action_entity.update(app, |pane, cx| {
                    if select_context_entry(
                        pane,
                        side,
                        action_entry.clone(),
                        &action_names,
                        context_generation,
                        &action_directory,
                        cx,
                    ) {
                        pane.delete_selected(cx);
                    }
                });
            }),
        );

        let action_entity = context_entity.clone();
        let action_entry = context_entry.clone();
        let action_directory = context_directory.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text("在编辑器中打开", "Open in editor"))
                .disabled(context_entry.kind != FileEntryKind::File)
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if pane.browser_action_snapshot_is_current(
                            side,
                            context_generation,
                            &action_directory,
                        ) {
                            cx.emit(super::super::OpenFileRequested {
                                endpoint: action_entry.endpoint.clone(),
                            });
                        }
                    });
                }),
        );
        menu
    })
    .into_any_element()
}

/// 右键动作使用打开菜单时捕获的行和列表顺序，不读取之后可能变化的键盘选择。
///
/// @author kongweiguang
fn select_context_entry(
    pane: &mut FilesPane,
    side: usize,
    entry: FileEntry,
    ordered_names: &[String],
    generation: u64,
    directory: &FileEndpoint,
    cx: &mut Context<FilesPane>,
) -> bool {
    if !pane.browser_action_snapshot_is_current(side, generation, directory) {
        return false;
    }
    pane.select_entry(
        side,
        entry,
        super::super::selection::SelectionGesture::Replace,
        ordered_names,
        cx,
    );
    true
}

/// Local/SFTP 使用 Unix 纳秒，容器 stat 使用整秒；按端点单位解码并拒绝非法日历值。
///
/// @author kongweiguang
fn file_modified_timestamp(entry: &FileEntry) -> Option<DateTime<Utc>> {
    entry
        .modified
        .as_deref()
        .and_then(|value| value.parse::<i128>().ok())
        .and_then(|value| {
            let (seconds, nanos) = if matches!(&entry.endpoint, FileEndpoint::Container { .. }) {
                (value, 0)
            } else {
                (
                    value.div_euclid(1_000_000_000),
                    u32::try_from(value.rem_euclid(1_000_000_000)).ok()?,
                )
            };
            DateTime::<Utc>::from_timestamp(i64::try_from(seconds).ok()?, nanos)
        })
        .filter(|timestamp| (1..=9999).contains(&timestamp.year()))
}

/// 列表以月日和时分节省横向空间；表头与悬浮详情明确标注 UTC。
///
/// @author kongweiguang
fn format_file_modified(entry: &FileEntry) -> String {
    file_modified_timestamp(entry)
        .map(|timestamp| timestamp.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "—".to_owned())
}

/// 悬浮详情补回年份和时区，短列值不会让人误把服务器时间当成本机时间。
///
/// @author kongweiguang
fn format_file_modified_tooltip(entry: &FileEntry) -> String {
    file_modified_timestamp(entry)
        .map(|timestamp| timestamp.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| locale::text("修改时间不可用", "Modified time unavailable").to_string())
}

/// 空目录的新建入口保留图标与对象文字，让首次使用者无需悬停即可理解；仍复用既有创建动作。
///
/// @author kongweiguang
pub(super) fn render_empty_directory(
    side: usize,
    hidden_only: bool,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> impl IntoElement {
    div()
        .flex_1()
        .w_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_1()
        .text_color(theme.subtle)
        .child(
            div()
                .w(px(34.))
                .h(px(34.))
                .mb_1()
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .bg(theme.secondary)
                .text_size(theme.text_heading)
                .text_color(theme.accent)
                .child(IconName::FolderOpen),
        )
        .child(
            div()
                .text_size(theme.text_sm)
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .text_color(theme.foreground)
                .child(if hidden_only {
                    locale::text("当前没有可见项目", "No visible items")
                } else {
                    locale::text("此目录为空", "This directory is empty")
                }),
        )
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.subtle)
                .child(if hidden_only {
                    locale::text(
                        "此目录包含隐藏项目。",
                        "This directory contains hidden items.",
                    )
                } else {
                    locale::text(
                        "可以新建文件夹，或从另一侧复制文件。",
                        "Create a folder or copy files from the other pane.",
                    )
                }),
        )
        .child(if hidden_only {
            Button::new(format!("files-empty-show-hidden-{side}"))
                .compact()
                .ghost()
                .label(locale::text("显示隐藏项目", "Show hidden items"))
                .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| pane.toggle_hidden_files(cx)))
        } else {
            Button::new(format!("files-empty-create-{side}"))
                .compact()
                .ghost()
                .icon(IconName::Plus)
                .label(locale::text("新建文件夹", "New folder"))
                .tooltip(locale::text(
                    "在当前目录新建文件夹",
                    "Create a folder in this directory",
                ))
                .on_click(cx.listener(move |pane, _: &ClickEvent, window, cx| {
                    pane.activate_side(side, cx);
                    pane.prepare_create_directory(window, cx);
                }))
        })
}

/// 未读取状态不伪装为空目录，提示用户从路径栏刷新重试。
///
/// @author kongweiguang
pub(super) fn render_directory_state(
    icon: IconName,
    title: gpui_kit::SharedString,
    detail: gpui_kit::SharedString,
    accent_icon: bool,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .flex_1()
        .w_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_1()
        .text_color(theme.subtle)
        .child(
            div()
                .w(px(32.))
                .h(px(32.))
                .mb_1()
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .bg(theme.secondary)
                .text_size(theme.text_sm)
                .text_color(if accent_icon {
                    theme.accent
                } else {
                    theme.subtle
                })
                .child(icon),
        )
        .child(
            div()
                .text_size(theme.text_sm)
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .text_color(theme.foreground)
                .child(title),
        )
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.subtle)
                .child(detail),
        )
}

/// 目录类型在前、同类按大小写无关名称排序，列表结构不受远端返回顺序影响。
///
/// @author kongweiguang
pub(in crate::features::files) fn entry_sort_key(entry: &FileEntry) -> u8 {
    match entry.kind {
        FileEntryKind::Directory => 0,
        FileEntryKind::File => 1,
        FileEntryKind::Symlink => 2,
        FileEntryKind::Other => 3,
    }
}

/// 文件类型仍分组优先，选中列只决定组内顺序；缺失元数据按 Option 稳定排列。
///
/// @author kongweiguang
pub(in crate::features::files) fn compare_entries(
    left: &FileEntry,
    right: &FileEntry,
    sort: FileSort,
) -> Ordering {
    let kind_order = entry_sort_key(left).cmp(&entry_sort_key(right));
    if kind_order != Ordering::Equal {
        return kind_order;
    }
    let primary = match sort.column {
        FileSortColumn::Name => left.name.to_lowercase().cmp(&right.name.to_lowercase()),
        FileSortColumn::Size => left.size.cmp(&right.size),
        FileSortColumn::Modified => {
            let left_modified = left
                .modified
                .as_deref()
                .and_then(|value| value.parse::<i128>().ok());
            let right_modified = right
                .modified
                .as_deref()
                .and_then(|value| value.parse::<i128>().ok());
            left_modified.cmp(&right_modified)
        }
    };
    let primary = if sort.descending {
        primary.reverse()
    } else {
        primary
    };
    primary
        .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        .then_with(|| left.name.cmp(&right.name))
}

/// 把 endpoint 的路径部分单独展示，连接身份由面板标题负责表达。
///
/// @author kongweiguang
pub(in crate::features::files) fn endpoint_path(endpoint: &FileEndpoint) -> String {
    match endpoint {
        FileEndpoint::Local { path } => path.to_string_lossy().into_owned(),
        FileEndpoint::Remote { path, .. } | FileEndpoint::Container { path, .. } => {
            path.as_str().to_owned()
        }
    }
}

/// 根据当前端点解析路径草稿；相对路径相对于当前目录，身份始终沿用原 endpoint。
///
/// @author kongweiguang
pub(in crate::features::files) fn endpoint_for_path(
    endpoint: &FileEndpoint,
    draft: &str,
) -> AppResult<FileEndpoint> {
    let draft = draft.trim();
    if draft.is_empty() {
        return Err(AppError::invalid("文件路径不能为空"));
    }
    match endpoint {
        FileEndpoint::Local { path } => {
            let target = PathBuf::from(draft);
            let target = if target.is_absolute() {
                target
            } else {
                path.join(target)
            };
            Ok(FileEndpoint::Local { path: target })
        }
        FileEndpoint::Remote { host_id, path } => {
            let target = resolve_remote_path(path.as_str(), draft, false)?;
            Ok(FileEndpoint::Remote {
                host_id: host_id.clone(),
                path: target,
            })
        }
        FileEndpoint::Container {
            host_id,
            runtime,
            container_id,
            path,
        } => {
            let target = resolve_remote_path(path.as_str(), draft, true)?;
            Ok(FileEndpoint::Container {
                host_id: host_id.clone(),
                runtime: *runtime,
                container_id: container_id.clone(),
                path: target,
            })
        }
    }
}

/// 远端相对路径以当前目录为基准归一化，容器路径额外拒绝越过容器根目录。
///
/// @author kongweiguang
fn resolve_remote_path(base: &str, draft: &str, container: bool) -> AppResult<RemotePath> {
    let combined = if draft.starts_with('/') || base == "." || base.is_empty() {
        draft.to_owned()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), draft)
    };
    let parsed = if container {
        RemotePath::parse_container(&combined)
    } else {
        normalize_remote_path(&combined)
    };
    parsed.map_err(|_| AppError::invalid("远端路径格式无效或越过根目录"))
}

/// 归一化 POSIX SFTP 路径段，使路径跳转与目录项导航保持相同的父子语义。
///
/// @author kongweiguang
fn normalize_remote_path(value: &str) -> Result<RemotePath, kerminal_domain::files::FilePathError> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return RemotePath::parse(value);
    }
    let absolute = value.starts_with('/');
    let mut segments = Vec::<&str>::new();
    for segment in value.split('/') {
        match segment {
            "" | "." => {}
            ".." if segments.last().is_some_and(|last| *last != "..") => {
                segments.pop();
            }
            ".." if absolute => {}
            value => segments.push(value),
        }
    }
    let joined = segments.join("/");
    let normalized = if absolute {
        if joined.is_empty() {
            "/".to_owned()
        } else {
            format!("/{joined}")
        }
    } else if joined.is_empty() {
        ".".to_owned()
    } else {
        joined
    };
    RemotePath::parse(&normalized)
}

/// 隐藏项优先遵循 FileService 属性；旧端点回传缺少该位时仍将 POSIX 点文件识别为隐藏。
///
/// @author kongweiguang
pub(in crate::features::files) fn is_hidden_entry(entry: &FileEntry) -> bool {
    entry.is_hidden || entry.name.starts_with('.')
}

#[cfg(test)]
#[path = "../../../../tests/unit/files_browser_projection.rs"]
mod tests;

#[cfg(test)]
#[path = "../../../../tests/unit/files_browser_metadata.rs"]
mod metadata_tests;
