//! 文件双栏面板的路径栏、工具栏和目录列表。该模块只投影现有 FileService 状态。
//!
//! @author kongweiguang

use chrono::{DateTime, Datelike, Local, Utc};
use gpui_kit::{
    AnyElement, App, ClickEvent, Context, DispatchPhase, ExternalPaths, InteractiveElement as _,
    IntoElement, KeyDownEvent, MouseDownEvent, Pixels, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement as _, Window,
    base::v_virtual_list,
    canvas,
    component::{
        ActiveTheme as _, Disableable as _, Icon, IconName,
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
use super::super::{FileSort, FileSortColumn};
use super::{FilesPane, asset_icon_button, icon_button, icon_button_without_tooltip};

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
    total_entry_count: usize,
    compact_layout: bool,
    show_hidden_files: bool,
    view_mode: super::super::view_mode::FilesViewMode,
    can_go_back: bool,
    can_go_forward: bool,
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
    compact_layout: bool,
    row_height: f32,
    show_hidden_files: bool,
    theme: ThemeTokens,
}

/// 表头与目录行按 main 的列表容器断点显隐列：时间常驻，大小 400px 起，权限 540px 起。
///
/// @author kongweiguang
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileTableColumns {
    size: bool,
    modified: bool,
    permissions: bool,
}

/// 目录面板尺寸决定表格列，避免缩窄时压缩大小和权限等辨识字段。
///
/// @author kongweiguang
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileBrowserLayout {
    columns: FileTableColumns,
}

/// 路径栏控件只接收本次渲染所需快照，避免把导航状态和主题拆成脆弱的位置参数。
///
/// @author kongweiguang
struct FilePathBarView<'a> {
    endpoint: &'a FileEndpoint,
    path_input: &'a gpui_kit::Entity<gpui_kit::component::input::InputState>,
    side: usize,
    compact_layout: bool,
    show_terminal_follow: bool,
    bookmarks: Vec<super::super::FileBookmark>,
    bookmarks_loading: bool,
    bookmark_error: Option<String>,
    current_path_bookmarkable: bool,
    current_path_is_bookmarked: bool,
    follow_terminal_directory: bool,
    can_setup_directory_follow: bool,
    directory_follow_setup_busy: bool,
    theme: ThemeTokens,
}

/// 书签菜单的四种互斥状态，加载中必须优先于缓存项或旧错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BookmarkMenuState<'a> {
    Loading,
    Error(&'a str),
    Empty,
    Items(&'a [super::super::FileBookmark]),
}

/// 将请求状态压成唯一菜单分支，避免旧错误或成功缓存遮住当前加载结果。
///
/// @author kongweiguang
fn bookmark_menu_state<'a>(
    loading: bool,
    error: Option<&'a str>,
    bookmarks: &'a [super::super::FileBookmark],
) -> BookmarkMenuState<'a> {
    if loading {
        BookmarkMenuState::Loading
    } else if let Some(error) = error {
        BookmarkMenuState::Error(error)
    } else if bookmarks.is_empty() {
        BookmarkMenuState::Empty
    } else {
        BookmarkMenuState::Items(bookmarks)
    }
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
    /// 保留 main 默认的时间列，并复用 400px/540px 的大小与权限列断点。
    ///
    /// @author kongweiguang
    fn for_width(width: Pixels) -> Self {
        Self {
            size: width >= px(400.),
            modified: true,
            permissions: width >= px(540.),
        }
    }
}

impl FileBrowserLayout {
    /// 列宽按面板实际尺寸计算，工具栏命令保持固定图标尺寸。
    ///
    /// @author kongweiguang
    fn for_width(width: Pixels) -> Self {
        Self {
            columns: FileTableColumns::for_width(width),
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
/// 紧凑右栏复用 rail 自身外壳，独立边框只属于文件列表卡；中央工作区继续用 endpoint 边界显示活动面板。
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
                .all(|(left, right)| left.endpoint == right.endpoint)
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
    let current_bookmark_target = bookmark_key.as_ref().map(|(target, _)| target);
    let bookmarks_loaded = current_bookmark_target
        .is_some_and(|target| pane.bookmarks_target.as_ref() == Some(target));
    let bookmarks_loading = current_bookmark_target
        .is_some_and(|target| pane.bookmark_request_target.as_ref() == Some(target));
    let bookmark_error = current_bookmark_target
        .filter(|target| pane.bookmark_error_target.as_ref() == Some(*target))
        .and_then(|_| pane.bookmark_error.clone());
    let path_bookmarks = if bookmarks_loaded {
        pane.bookmarks
            .iter()
            .filter(|bookmark| super::super::bookmark_endpoint(&endpoint, bookmark).is_some())
            .cloned()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

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
        .when(!pane.compact_layout, |panel| {
            panel.rounded_md().border_1().border_color(if active {
                theme.accent.opacity(0.55)
            } else {
                theme.border
            })
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
            compact_layout: pane.compact_layout,
            show_terminal_follow: toolbar_projection.show_terminal_follow,
            bookmarks: path_bookmarks,
            bookmarks_loading,
            bookmark_error,
            current_path_bookmarkable,
            current_path_is_bookmarked,
            follow_terminal_directory: pane.follow_terminal_directory,
            can_setup_directory_follow: pane.current_terminal_binding.is_some(),
            directory_follow_setup_busy: pane
                .current_terminal_binding
                .as_ref()
                .is_some_and(|binding| pane.directory_follow_setup_bindings.contains(binding)),
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
            total_entry_count: all_count,
            compact_layout: pane.compact_layout,
            show_hidden_files,
            view_mode: pane.view_mode,
            loading,
            can_go_back: panel.navigation_history.can_go_back(),
            can_go_forward: panel.navigation_history.can_go_forward(),
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
    let table_heading = render_table_heading(
        side,
        panel.sort,
        theme,
        if tree_mode {
            FileTableColumns {
                size: false,
                modified: false,
                permissions: false,
            }
        } else {
            columns
        },
        pane.compact_layout,
        !tree_mode,
        cx,
    )
    .into_any_element();

    if tree_mode {
        return panel_view
            .child(table_heading)
            .child(super::tree::render_tree_content(
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
        .debug_selector(move || format!("files-browser-list-{side}"))
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
            "Home End Enter Space Control+A Meta+A Delete F2",
        ))
        .on_key_down(cx.listener(move |pane, event: &KeyDownEvent, window, cx| {
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
            } else if key == "delete" && !pane.selected_entries(side).is_empty() {
                pane.activate_side(side, cx);
                pane.delete_selected(cx);
                cx.stop_propagation();
            } else if key == "f2" && pane.selected_entries(side).len() == 1 {
                pane.activate_side(side, cx);
                pane.prepare_rename(window, cx);
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

    let trust_request = pane.current_host_key_trust_request();
    let host_key_trust_busy = trust_request.as_ref().is_some_and(|request| {
        endpoint.host_id() == Some(&request.host_id)
            && matches!(&endpoint, FileEndpoint::Remote { .. })
    });
    if loading {
        list = list.child(render_directory_state(
            IconName::LoaderCircle,
            locale::text("正在读取目录", "Reading directory"),
            locale::text(
                "文件与权限信息将显示在这里。",
                "Files and permissions will appear here.",
            ),
            true,
            None,
            theme,
        ));
    } else if listing.is_none() {
        let trust_button = (pane.compact_layout
            && side == 1
            && panel.directory_error.is_some()
            && matches!(&endpoint, FileEndpoint::Remote { .. }))
        .then(|| {
            Button::new("files-trust-host-key")
                .compact()
                .danger()
                .icon(IconName::Check)
                .label(locale::text("信任主机密钥", "Trust host key"))
                .disabled(host_key_trust_busy)
                .loading(host_key_trust_busy)
                .accessibility_label(locale::text(
                    "信任 SFTP 主机密钥",
                    "Trust the SFTP host key",
                ))
                .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                    pane.request_host_key_trust(cx);
                }))
                .into_any_element()
        });
        let (title, detail) = panel.directory_error.as_ref().map_or_else(
            || {
                (
                    locale::text("目录尚未读取", "Directory not loaded"),
                    locale::text(
                        "刷新即可重新读取当前路径。",
                        "Refresh to read the current path.",
                    ),
                )
            },
            |error| {
                (
                    locale::text("无法读取目录", "Could not read directory"),
                    error.clone().into(),
                )
            },
        );
        list = list.child(render_directory_state(
            IconName::FolderOpen,
            title,
            detail,
            false,
            trust_button,
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
                None,
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
        let row_height = pane.file_row_height;
        let row_sizes = Rc::new(vec![size(px(1.), px(row_height)); rows.len()]);
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
        // 虚拟行回调跨帧保留；只捕获布局值，不能把当前 FilesPane 的可变借用带出 render。
        let compact_layout = pane.compact_layout;
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
                                compact_layout,
                                row_height,
                                show_hidden_files,
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
        menu = crate::ui_popup::context_menu(menu, theme, None);
        let action_entity = background_entity.clone();
        let action_endpoint = background_endpoint.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text(
                "工作区打开当前目录",
                "Open current directory in workspace",
            ))
            .icon(Icon::new(AssetIconName::FileSearch))
            .on_click(move |_, _, app| {
                action_entity.update(app, |pane, cx| {
                    if pane.browser_action_snapshot_is_current(
                        side,
                        background_generation,
                        &action_endpoint,
                    ) {
                        pane.activate_side(side, cx);
                        pane.set_file_view_mode(
                            super::super::view_mode::FilesViewMode::Workspace,
                            cx,
                        );
                    }
                });
            }),
        );

        if !matches!(&background_endpoint, FileEndpoint::Local { .. }) {
            let action_entity = background_entity.clone();
            let action_endpoint = background_endpoint.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("上传文件", "Upload files"))
                    .icon(Icon::new(AssetIconName::Upload))
                    .on_click(move |_, _, app| {
                        action_entity.update(app, |pane, cx| {
                            if pane.browser_action_snapshot_is_current(
                                side,
                                background_generation,
                                &action_endpoint,
                            ) {
                                pane.prompt_upload_files_to(action_endpoint.clone(), cx);
                            }
                        });
                    }),
            );

            let action_entity = background_entity.clone();
            let action_endpoint = background_endpoint.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("上传文件夹", "Upload folder"))
                    .icon(IconName::FolderOpen)
                    .on_click(move |_, _, app| {
                        action_entity.update(app, |pane, cx| {
                            if pane.browser_action_snapshot_is_current(
                                side,
                                background_generation,
                                &action_endpoint,
                            ) {
                                pane.prompt_upload_directories_to(action_endpoint.clone(), cx);
                            }
                        });
                    }),
            );
        }

        if matches!(&background_endpoint, FileEndpoint::Remote { .. }) {
            let action_entity = background_entity.clone();
            let action_endpoint = background_endpoint.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("粘贴 SFTP 剪贴板", "Paste SFTP clipboard"))
                    .icon(Icon::new(AssetIconName::ClipboardPaste))
                    .on_click(move |_, _, app| {
                        action_entity.update(app, |pane, cx| {
                            if pane.browser_action_snapshot_is_current(
                                side,
                                background_generation,
                                &action_endpoint,
                            ) {
                                pane.paste_remote_clipboard(action_endpoint.clone(), cx);
                            }
                        });
                    }),
            );
        }

        let action_entity = background_entity.clone();
        let action_endpoint = background_endpoint.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text("新建目录", "New folder"))
                .icon(Icon::new(AssetIconName::FolderPlus))
                .on_click(move |_, window, app| {
                    action_entity.update(app, |pane, cx| {
                        if pane.browser_action_snapshot_is_current(
                            side,
                            background_generation,
                            &action_endpoint,
                        ) {
                            pane.activate_side(side, cx);
                            pane.prepare_create_directory(window, cx);
                        }
                    });
                }),
        );
        menu = menu.item(PopupMenuItem::separator());

        let action_entity = background_entity.clone();
        let action_endpoint = background_endpoint.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text("刷新目录", "Refresh directory"))
                .icon(IconName::RefreshCw)
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if pane.browser_action_snapshot_is_current(
                            side,
                            background_generation,
                            &action_endpoint,
                        ) {
                            pane.refresh_panel(side, cx);
                        }
                    });
                }),
        );

        let action_entity = background_entity.clone();
        let action_endpoint = background_endpoint.clone();
        menu = menu.item(
            PopupMenuItem::new(if show_hidden_files {
                locale::text("隐藏隐藏文件", "Hide hidden files")
            } else {
                locale::text("显示隐藏文件", "Show hidden files")
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
            PopupMenuItem::new(locale::text("复制当前路径", "Copy current path"))
                .icon(IconName::Copy)
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if pane.browser_action_snapshot_is_current(
                            side,
                            background_generation,
                            &action_endpoint,
                        ) {
                            pane.copy_path_to_clipboard(&action_endpoint, cx);
                        }
                    });
                }),
        )
    });

    if pane.compact_layout {
        panel_view.child(
            div()
                .id(format!("files-browser-entry-card-{side}"))
                .debug_selector(move || format!("files-browser-entry-card-{side}"))
                .mx_2()
                .mt_2()
                .mb_2()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_col()
                .overflow_hidden()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.panel)
                .child(table_heading)
                .child(list),
        )
    } else {
        panel_view.child(table_heading).child(list)
    }
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

/// 紧凑路径栏复用 main 的实色圆角外壳与紧凑节奏；按钮和 Enter 仍共用同一导航校验。
/// 书签空态保留保存路径的提示，使图标操作在无数据时也可发现。
/// 紧凑单行高度经 Styled 设置，绕开 Input 的多行专用 h，并保留中央路径栏原尺寸。
///
/// @author kongweiguang
fn render_path_bar(view: FilePathBarView<'_>, cx: &mut Context<FilesPane>) -> impl IntoElement {
    let FilePathBarView {
        endpoint,
        path_input,
        side,
        compact_layout,
        show_terminal_follow,
        bookmarks,
        bookmarks_loading,
        bookmark_error,
        current_path_bookmarkable,
        current_path_is_bookmarked,
        follow_terminal_directory,
        can_setup_directory_follow,
        directory_follow_setup_busy,
        theme,
    } = view;
    let scheme = match endpoint {
        FileEndpoint::Local { .. } => "file://",
        FileEndpoint::Remote { .. } => "sftp://",
        FileEndpoint::Container { .. } => "container://",
    };
    let full_path = endpoint_path(endpoint);
    let system_font_family = cx.theme().font_family.clone();
    let path_field = div()
        .id(format!("files-path-{side}"))
        .min_w(px(0.))
        .h(if compact_layout { px(34.) } else { px(32.) })
        .flex()
        .items_center()
        .gap_2()
        .overflow_hidden()
        .when(!compact_layout, |field| {
            field
                .px_3()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
        })
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
                        .h(if compact_layout { px(34.) } else { px(30.) })
                        .text_size(if compact_layout {
                            px(16.)
                        } else {
                            theme.text_sm
                        })
                        .when(compact_layout, |input| {
                            gpui_kit::Styled::h(input, px(34.)).font_family(system_font_family)
                        }),
                ),
        );

    let jump = Button::new(format!("files-path-jump-{side}"))
        .compact()
        .w(px(32.))
        .h(px(32.))
        .flex_shrink_0()
        .ghost()
        .when(compact_layout, Button::outline)
        .icon(IconName::ArrowRight)
        .tooltip(locale::text(
            "跳转到输入路径（Enter）",
            "Go to entered path (Enter)",
        ))
        .accessibility_label(locale::text("跳转到输入路径", "Go to entered path"))
        .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| pane.commit_path_edit(side, cx)));
    let bookmark_toggle = Button::new(format!("files-bookmark-toggle-{side}"))
        .compact()
        .w(px(32.))
        .h(px(32.))
        .when(current_path_is_bookmarked, Button::primary)
        .when(!current_path_is_bookmarked, Button::ghost)
        .icon(AssetIconName::Bookmark)
        .tooltip(locale::text(
            if current_path_is_bookmarked {
                "取消收藏当前路径"
            } else {
                "收藏当前路径"
            },
            if current_path_is_bookmarked {
                "Remove bookmark for this path"
            } else {
                "Bookmark this path"
            },
        ))
        .accessibility_label(locale::text("收藏当前路径", "Bookmark current path"))
        .disabled(!current_path_bookmarkable)
        .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
            pane.toggle_bookmark(side, cx);
        }));
    let current_endpoint = endpoint.clone();
    let entity = cx.entity().clone();
    let bookmark_menu_button = Button::new(format!("files-bookmark-menu-{side}"))
        .debug_selector(move || format!("rr-files-bookmarks-{side}"))
        .compact()
        .w(px(24.))
        .h(px(32.))
        .ghost()
        .icon(IconName::ChevronDown)
        .tooltip(locale::text("路径书签", "Path bookmarks"))
        .accessibility_label(locale::text("打开路径书签", "Open path bookmarks"))
        .dropdown_menu_with_anchor(gpui_kit::Anchor::TopRight, move |mut menu, window, _| {
            menu = crate::ui_popup::file_bookmark_menu(menu, theme, window.viewport_size().width);
            menu = menu.item(
                PopupMenuItem::label(locale::text("路径书签", "Path bookmarks")).disabled(true),
            );
            match bookmark_menu_state(bookmarks_loading, bookmark_error.as_deref(), &bookmarks) {
                BookmarkMenuState::Loading => {
                    menu = menu.item(
                        PopupMenuItem::label(locale::text("正在读取书签…", "Loading bookmarks…"))
                            .disabled(true),
                    );
                }
                BookmarkMenuState::Error(error) => {
                    menu = menu.item(PopupMenuItem::label(error.to_owned()).disabled(true));
                }
                BookmarkMenuState::Empty => {
                    menu = menu.item(
                        PopupMenuItem::label(locale::text(
                            "暂无书签；点击书签图标保存当前路径。",
                            "No bookmarks yet; click the bookmark icon to save the current path.",
                        ))
                        .disabled(true),
                    );
                }
                BookmarkMenuState::Items(bookmarks) => {
                    menu = menu.item(PopupMenuItem::separator());
                    for bookmark in bookmarks.iter().cloned() {
                        let Some(destination) =
                            super::super::bookmark_endpoint(&current_endpoint, &bookmark)
                        else {
                            continue;
                        };
                        let path = bookmark.path.clone();
                        let display_path = path.clone();
                        let remove_path = bookmark.clone();
                        let navigate_entity = entity.clone();
                        let remove_entity = entity.clone();
                        menu = menu.item(
                            PopupMenuItem::element(move |_, _| {
                                let remove_entity = remove_entity.clone();
                                let remove_path = remove_path.clone();
                                let path = display_path.clone();
                                div()
                                    .flex()
                                    .min_w(px(0.))
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        div()
                                            .min_w(px(0.))
                                            .flex_1()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .child(path.clone()),
                                    )
                                    .child(
                                        Button::new(format!("files-bookmark-remove-{side}-{path}"))
                                            .compact()
                                            .ghost()
                                            .w(px(28.))
                                            .h(px(28.))
                                            .flex_shrink_0()
                                            .icon(Icon::new(AssetIconName::Trash))
                                            .tooltip(locale::text("删除书签", "Remove bookmark"))
                                            .accessibility_label(locale::text(
                                                format!("删除书签 {path}"),
                                                format!("Remove bookmark {path}"),
                                            ))
                                            .on_click(move |_, _, app| {
                                                app.stop_propagation();
                                                remove_entity.update(app, |pane, cx| {
                                                    pane.remove_bookmark(
                                                        side,
                                                        remove_path.clone(),
                                                        cx,
                                                    );
                                                });
                                            }),
                                    )
                            })
                            .on_click(move |_, _, app| {
                                navigate_entity.update(app, |pane, cx| {
                                    pane.open_endpoint_on_side(side, destination.clone(), cx);
                                });
                            }),
                        );
                    }
                }
            }
            menu
        })
        .offset(px(6.));

    let follow = show_terminal_follow.then(|| {
        Button::new(format!("files-follow-terminal-{side}"))
            .compact()
            .w(px(32.))
            .h(px(32.))
            .outline()
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
    let setup_follow = (show_terminal_follow && can_setup_directory_follow).then(|| {
        Button::new(format!("files-setup-follow-terminal-{side}"))
            .compact()
            .w(px(32.))
            .h(px(32.))
            .ghost()
            .outline()
            .icon(IconName::Settings2)
            .loading(directory_follow_setup_busy)
            .disabled(directory_follow_setup_busy)
            .tooltip(locale::text(
                "自动配置目录跟随",
                "Automatically configure directory tracking",
            ))
            .accessibility_label(locale::text(
                "自动配置 SFTP 目录跟随",
                "Set up SFTP directory tracking",
            ))
            .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                pane.request_directory_follow_setup(cx);
            }))
    });
    let bookmark_control = div()
        .h(px(32.))
        .flex()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .child(bookmark_toggle)
        .child(div().w(px(1.)).h(px(20.)).bg(theme.border))
        .child(bookmark_menu_button);

    let mut path_controls = div()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .child(path_field.flex_1())
        .child(bookmark_control)
        .child(jump)
        .when_some(follow, |bar, follow| bar.child(follow))
        .when_some(setup_follow, |bar, setup| bar.child(setup));
    if compact_layout {
        path_controls = path_controls
            .h(px(50.))
            .px(px(10.))
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.tool_panel_surface);
    }

    div()
        .id(format!("files-path-bar-{side}"))
        .w_full()
        .h(if compact_layout { px(66.) } else { px(48.) })
        .flex_shrink_0()
        .px(if compact_layout { px(8.) } else { px(12.) })
        .flex()
        .items_center()
        .when(!compact_layout, |bar| {
            bar.border_b_1().border_color(theme.border)
        })
        .bg(theme.panel)
        .child(path_controls)
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

/// GPUI Kit 的 ghost+outline 默认背景和边框透明；局部覆盖语义中性令牌并淡化禁用态以匹配 main。
/// @author kongweiguang
fn compact_toolbar_button(button: Button, theme: ThemeTokens, disabled: bool) -> Button {
    button
        .secondary()
        .outline()
        .bg(theme.muted_surface())
        .border_color(theme.tool_panel_border)
        .when(disabled, |button| button.opacity(0.45))
}

/// 紧凑栏按 main 顺序展示操作；路径外壳留出 main 的上下间距，按钮继续使用 32px 点击区。
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
        endpoint,
        selected_count,
        has_remote_selection,
        visible_count,
        total_entry_count,
        show_hidden_files,
        view_mode,
        ..
    } = view;
    let can_upload = !matches!(endpoint, FileEndpoint::Local { .. });

    let mode_entity = cx.entity().clone();
    let more = compact_toolbar_button(
        Button::new(format!("files-view-mode-{side}")).compact(),
        theme,
        false,
    )
    .w(px(32.))
    .h(px(32.))
    .flex_shrink_0()
    .icon(IconName::Ellipsis)
    .tooltip(locale::text("切换文件视图", "Switch file view"))
    .accessibility_label(locale::text("切换文件视图", "Switch file view"))
    .debug_selector(|| "rr-files-view-menu".into())
    .dropdown_menu_with_anchor(gpui_kit::Anchor::TopRight, move |mut menu, _, _| {
        menu = crate::ui_popup::file_view_menu(menu, theme);
        for (mode, label) in [
            (
                super::super::view_mode::FilesViewMode::List,
                locale::text("列表模式", "List mode"),
            ),
            (
                super::super::view_mode::FilesViewMode::Tree,
                locale::text("树形模式", "Tree mode"),
            ),
            (
                super::super::view_mode::FilesViewMode::Workspace,
                locale::text("工作区模式", "Workspace mode"),
            ),
        ] {
            let action_entity = mode_entity.clone();
            menu = menu.item(
                PopupMenuItem::new(label)
                    .icon(match mode {
                        super::super::view_mode::FilesViewMode::List => AssetIconName::List,
                        super::super::view_mode::FilesViewMode::Tree => AssetIconName::ListTree,
                        super::super::view_mode::FilesViewMode::Workspace => {
                            AssetIconName::PanelRight
                        }
                    })
                    .checked(view_mode == mode)
                    .on_click(move |_, _, app| {
                        action_entity.update(app, |pane, cx| pane.set_file_view_mode(mode, cx));
                    }),
            );
        }
        menu
    })
    .offset(px(6.));

    let parent_disabled = super::super::parent_endpoint(endpoint).is_none();
    let parent = icon_button(
        format!("files-parent-{side}"),
        IconName::ArrowUp,
        locale::text("上级目录", "Parent directory"),
        parent_disabled,
    );
    let parent = compact_toolbar_button(parent, theme, parent_disabled)
        .w(px(32.))
        .h(px(32.))
        .flex_shrink_0()
        .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| pane.navigate_parent(side, cx)));
    let refresh_disabled = loading;
    let refresh = icon_button(
        format!("files-refresh-{side}"),
        IconName::RefreshCw,
        locale::text("刷新目录", "Refresh directory"),
        refresh_disabled,
    );
    let refresh = compact_toolbar_button(refresh, theme, refresh_disabled)
        .w(px(32.))
        .h(px(32.))
        .flex_shrink_0()
        .loading(refresh_disabled)
        .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| pane.refresh_panel(side, cx)));

    let count_label = if loading {
        locale::text("刷新中", "Refreshing")
    } else if selected_count == 0 {
        locale::text(
            format!("{visible_count} / {total_entry_count} 项"),
            format!("{visible_count} / {total_entry_count} items"),
        )
    } else {
        locale::text(
            format!("{visible_count} / {total_entry_count} 项 · 已选 {selected_count}"),
            format!("{visible_count} / {total_entry_count} items · {selected_count} selected"),
        )
    };
    let count_tooltip = count_label.clone();

    let mut actions = div()
        .id(format!("files-actions-{side}"))
        .w_full()
        .min_h(px(40.))
        .flex_shrink_0()
        .px(px(8.))
        .py_1()
        .flex()
        .items_center()
        .gap(px(2.))
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(parent)
        .child(refresh);
    actions = actions
        .child(
            compact_toolbar_button(
                icon_button(
                    format!("files-toggle-hidden-{side}"),
                    if show_hidden_files {
                        IconName::EyeOff
                    } else {
                        IconName::Eye
                    },
                    if show_hidden_files {
                        locale::text("隐藏隐藏文件", "Hide hidden files")
                    } else {
                        locale::text("显示隐藏文件", "Show hidden files")
                    },
                    false,
                ),
                theme,
                false,
            )
            .w(px(32.))
            .h(px(32.))
            .when(show_hidden_files, |button| {
                button
                    .border_color(theme.accent.opacity(0.45))
                    .bg(theme.surface_selected())
                    .text_color(theme.accent)
            })
            .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| pane.toggle_hidden_files(cx))),
        )
        .child(div().w(px(1.)).h(px(16.)).flex_shrink_0().bg(theme.border));
    if can_upload {
        let upload_files_entity = cx.entity().clone();
        let upload_directories_entity = cx.entity().clone();
        actions = actions.child(
            compact_toolbar_button(
                asset_icon_button(
                    format!("files-upload-{side}"),
                    AssetIconName::Upload,
                    locale::text("从本机选择并上传", "Choose local files and upload"),
                    transfer_submission_busy,
                ),
                theme,
                transfer_submission_busy,
            )
            .w(px(32.))
            .h(px(32.))
            .flex_shrink_0()
            .debug_selector(|| "rr-files-upload-menu".into())
            .dropdown_menu(move |menu, _, _| {
                let menu = crate::ui_popup::file_upload_menu(menu, theme);
                let upload_files_entity = upload_files_entity.clone();
                let upload_directories_entity = upload_directories_entity.clone();
                menu.item(upload_menu_item(
                    format!("files-upload-menu-file-{side}"),
                    AssetIconName::Upload,
                    locale::text("上传文件", "Upload files"),
                    theme,
                    move |_, _, app| {
                        upload_files_entity.update(app, |pane, cx| {
                            pane.activate_side(side, cx);
                            pane.prompt_upload_files(cx);
                        });
                    },
                ))
                .item(upload_menu_item(
                    format!("files-upload-menu-folder-{side}"),
                    AssetIconName::FolderOpen,
                    locale::text("上传文件夹", "Upload folder"),
                    theme,
                    move |_, _, app| {
                        upload_directories_entity.update(app, |pane, cx| {
                            pane.activate_side(side, cx);
                            pane.prompt_upload_directories(cx);
                        });
                    },
                ))
            })
            .offset(px(6.)),
        );
    }
    actions = actions.child(
        compact_toolbar_button(
            asset_icon_button(
                format!("files-new-folder-{side}"),
                AssetIconName::FolderPlus,
                locale::text("在当前目录新建文件夹", "Create a folder in this directory"),
                false,
            ),
            theme,
            false,
        )
        .w(px(32.))
        .h(px(32.))
        .debug_selector(move || format!("files-new-folder-{side}"))
        .on_click(cx.listener(move |pane, _: &ClickEvent, window, cx| {
            pane.activate_side(side, cx);
            pane.prepare_create_directory(window, cx);
        })),
    );
    if can_upload {
        actions = actions.child(
            div()
                .id(format!("files-download-divider-{side}"))
                .debug_selector(move || format!("files-download-divider-{side}"))
                .w(px(1.))
                .h(px(16.))
                .flex_shrink_0()
                .bg(theme.border),
        );
        actions = actions.child(
            compact_toolbar_button(
                icon_button(
                    format!("files-download-{side}"),
                    IconName::ArrowDown,
                    locale::text("下载所选项目", "Download selected items"),
                    transfer_submission_busy || !has_remote_selection,
                ),
                theme,
                transfer_submission_busy || !has_remote_selection,
            )
            .w(px(32.))
            .h(px(32.))
            .flex_shrink_0()
            .debug_selector(move || format!("files-download-{side}"))
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
                .w_auto()
                .min_w(px(0.))
                .max_w(px(120.))
                .flex_1()
                .overflow_hidden()
                .text_ellipsis()
                .text_size(theme.text_xs)
                .text_color(theme.subtle)
                .aria_label(count_label.clone())
                .tooltip(move |window, cx| Tooltip::new(count_tooltip.clone()).build(window, cx))
                .child(count_label),
        )
        .child(more)
        .into_any_element()
}

/// 上传菜单行使用主题 sky 色的16px图标，同时沿用 PopupMenu 的点击、键盘和焦点行为。
///
/// @author kongweiguang
fn upload_menu_item(
    id: String,
    icon: AssetIconName,
    label: SharedString,
    theme: ThemeTokens,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> PopupMenuItem {
    let accessible_label = label.clone();
    PopupMenuItem::element(move |_, _| {
        div()
            .id(id.clone())
            .w_full()
            .h(px(32.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .rounded(theme.control_radius)
            .text_size(px(16.))
            .line_height(px(24.))
            .text_color(theme.foreground)
            .aria_label(accessible_label.clone())
            .child(
                Icon::new(icon)
                    .size(px(16.))
                    .text_color(theme.file_upload_menu_icon()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(label.clone()),
            )
    })
    .on_click(on_click)
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
            menu = crate::ui_popup::dropdown_menu(menu, theme);
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
        .offset(px(6.))
}

/// 表头与目录行共享固定元数据宽度；紧凑右栏跟随 main 的本机时间格式，中央工作区保留 UTC 标示。
///
/// @author kongweiguang
fn render_table_heading(
    side: usize,
    sort: FileSort,
    theme: ThemeTokens,
    columns: FileTableColumns,
    compact_layout: bool,
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
    let mut name_heading = div().flex().items_center().gap_1().child(
        div()
            .when(sortable, |label| label.pl(px(24.)))
            .child(name_label),
    );
    name_heading = name_heading.flex_1().min_w(px(0.));
    let mut heading = div()
        .w_full()
        .h(px(28.))
        .flex_shrink_0()
        .px(px(10.))
        .flex()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .text_size(px(12.))
        .text_color(theme.subtle)
        .child(name_heading);
    if columns.permissions {
        heading = heading.child(
            div()
                .w(px(76.))
                .flex_shrink_0()
                .text_right()
                .text_size(px(12.))
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
                width: Some(px(68.)),
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
                label: if compact_layout {
                    locale::text("时间", "Time")
                } else {
                    locale::text("时间 · UTC", "Time · UTC")
                },
                sort,
                width: Some(px(92.)),
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

/// 元数据列挤压文件名时保留完整名称提示；紧凑行跟随 main 的本机时间，中央工作区继续显示 UTC。
/// 实际行与虚拟列表共用同一密度行高，防止滚动几何错位。
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
        compact_layout,
        row_height,
        show_hidden_files,
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
    let modified = format_file_modified(&entry, compact_layout);
    let modified_tooltip = format_file_modified_tooltip(&entry, compact_layout);
    let name = entry.name.clone();
    let name_tooltip = name.clone();
    let context_entry = entry.clone();
    let can_change_permissions =
        matches!(entry.kind, FileEntryKind::File | FileEntryKind::Directory)
            && entry.permissions.is_some()
            && super::super::permissions::supports_posix_mode_endpoint(&entry.endpoint);
    let context_entity = cx.entity().clone();
    let context_names = Arc::clone(&ordered_names);
    let context_selected_entries = Arc::clone(&selected_drag_entries);
    let context_multi_selection = selected && context_selected_entries.len() > 1;
    let context_directory = source_directory.clone();
    let context_generation = source_generation;
    let accessible_name = format!("{}, {}", entry.name, accessible_kind);
    let row_selector = format!("files-row-{side}-{name}");
    let row_debug_selector = row_selector.clone();
    let click_entry = entry.clone();
    let double_click_entry = entry.clone();
    let click_names = Arc::clone(&ordered_names);
    let click_directory = source_directory.clone();
    let click_generation = source_generation;
    let mut row = div()
        .id(row_selector)
        .debug_selector(move || row_debug_selector.clone())
        .w_full()
        .h(px(row_height))
        .flex_shrink_0()
        .px(px(10.))
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
                .ml(px(4.))
                .mr(px(2.))
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
        .child(
            div()
                .w(px(16.))
                .mr(px(2.))
                .text_color(theme.accent)
                .child(kind_icon),
        )
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
                .w(px(76.))
                .flex_shrink_0()
                .text_right()
                .text_size(px(11.))
                .text_color(theme.subtle)
                .font_family("monospace")
                .child(mode),
        );
    }
    if columns.size {
        row = row.child(
            div()
                .w(px(68.))
                .flex_shrink_0()
                .text_right()
                .text_size(px(11.))
                .font_family("monospace")
                .text_color(theme.subtle)
                .child(size),
        );
    }
    if columns.modified {
        row = row.child(
            div()
                .id(format!("files-modified-{side}-{index}"))
                .w(px(92.))
                .flex_shrink_0()
                .overflow_hidden()
                .text_right()
                .text_size(px(11.))
                .font_family("monospace")
                .text_color(theme.subtle)
                .whitespace_nowrap()
                .text_ellipsis()
                .tooltip(move |window, cx| Tooltip::new(modified_tooltip.clone()).build(window, cx))
                .child(modified),
        );
    }
    row.context_menu(move |mut menu, _, _| {
        menu = crate::ui_popup::context_menu(menu, theme, None);
        if context_multi_selection {
            let transferable_count = context_selected_entries
                .iter()
                .filter(|entry| {
                    matches!(
                        entry.kind,
                        FileEntryKind::File | FileEntryKind::Directory | FileEntryKind::Symlink
                    ) && !matches!(&entry.endpoint, FileEndpoint::Local { .. })
                })
                .count();
            let selected_count = context_selected_entries.len();
            let action_entity = context_entity.clone();
            let action_selection = Arc::clone(&context_selected_entries);
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
                        if select_context_selection(
                            pane,
                            side,
                            &action_selection,
                            context_generation,
                            &action_directory,
                            cx,
                        ) {
                            pane.prompt_download(cx);
                        }
                    });
                }),
            );
            menu = menu.item(PopupMenuItem::separator());

            let action_entity = context_entity.clone();
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
                                pane.refresh_panel(side, cx);
                            }
                        });
                    }),
            );

            let action_entity = context_entity.clone();
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(if show_hidden_files {
                    locale::text("隐藏隐藏文件", "Hide hidden files")
                } else {
                    locale::text("显示隐藏文件", "Show hidden files")
                })
                .icon(if show_hidden_files {
                    IconName::EyeOff
                } else {
                    IconName::Eye
                })
                .checked(show_hidden_files)
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if pane.browser_action_snapshot_is_current(
                            side,
                            context_generation,
                            &action_directory,
                        ) {
                            pane.toggle_hidden_files(cx);
                        }
                    });
                }),
            );
            menu = menu.item(PopupMenuItem::separator());

            let action_entity = context_entity.clone();
            let action_selection = Arc::clone(&context_selected_entries);
            let action_directory = context_directory.clone();
            return menu.item(
                PopupMenuItem::new(locale::text(
                    format!("删除选中 {selected_count} 项"),
                    format!("Delete {selected_count} selected items"),
                ))
                .icon(Icon::new(AssetIconName::Trash))
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if select_context_selection(
                            pane,
                            side,
                            &action_selection,
                            context_generation,
                            &action_directory,
                            cx,
                        ) {
                            pane.delete_selected(cx);
                        }
                    });
                }),
            );
        }

        if context_entry.kind == FileEntryKind::Directory {
            let action_entry = context_entry.clone();
            let action_entity = context_entity.clone();
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("打开", "Open"))
                    .icon(IconName::FolderOpen)
                    .on_click(move |_, _, app| {
                        action_entity.update(app, |pane, cx| {
                            if pane.browser_action_snapshot_is_current(
                                side,
                                context_generation,
                                &action_directory,
                            ) {
                                pane.open_entry(side, action_entry.clone(), cx);
                            }
                        });
                    }),
            );
            let action_entry = context_entry.clone();
            let action_entity = context_entity.clone();
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("工作区打开", "Open in workspace"))
                    .icon(Icon::new(AssetIconName::FileSearch))
                    .on_click(move |_, _, app| {
                        action_entity.update(app, |pane, cx| {
                            if pane.browser_action_snapshot_is_current(
                                side,
                                context_generation,
                                &action_directory,
                            ) {
                                pane.activate_side(side, cx);
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
            let action_entity = context_entity.clone();
            let action_entry = context_entry.clone();
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("打开编辑器", "Open in editor"))
                    .icon(Icon::new(AssetIconName::FileSearch))
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
        }

        let action_entry = context_entry.clone();
        let action_entity = context_entity.clone();
        let action_names = Arc::clone(&context_names);
        let action_directory = context_directory.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text(
                if context_entry.kind == FileEntryKind::Directory {
                    "下载文件夹"
                } else {
                    "下载"
                },
                if context_entry.kind == FileEntryKind::Directory {
                    "Download folder"
                } else {
                    "Download"
                },
            ))
            .icon(Icon::new(AssetIconName::Download))
            .disabled(
                matches!(&context_entry.endpoint, FileEndpoint::Local { .. })
                    || !matches!(
                        context_entry.kind,
                        FileEntryKind::File | FileEntryKind::Directory | FileEntryKind::Symlink
                    ),
            )
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

        if matches!(&context_entry.endpoint, FileEndpoint::Remote { .. }) {
            let action_entry = context_entry.clone();
            let action_entity = context_entity.clone();
            let action_names = Arc::clone(&context_names);
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("下载到剪贴板", "Download to clipboard"))
                    .icon(Icon::new(AssetIconName::Download))
                    .disabled(!matches!(
                        context_entry.kind,
                        FileEntryKind::File | FileEntryKind::Directory | FileEntryKind::Symlink
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

        if context_entry.kind == FileEntryKind::Directory
            && !matches!(&context_entry.endpoint, FileEndpoint::Local { .. })
        {
            let action_entity = context_entity.clone();
            let action_endpoint = context_entry.endpoint.clone();
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text(
                    "上传文件到此目录",
                    "Upload files into this folder",
                ))
                .icon(Icon::new(AssetIconName::Upload))
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if pane.browser_action_snapshot_is_current(
                            side,
                            context_generation,
                            &action_directory,
                        ) {
                            pane.prompt_upload_files_to(action_endpoint.clone(), cx);
                        }
                    });
                }),
            );

            let action_entity = context_entity.clone();
            let action_endpoint = context_entry.endpoint.clone();
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text(
                    "上传文件夹到此目录",
                    "Upload folders into this folder",
                ))
                .icon(Icon::new(AssetIconName::FolderPlus))
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if pane.browser_action_snapshot_is_current(
                            side,
                            context_generation,
                            &action_directory,
                        ) {
                            pane.prompt_upload_directories_to(action_endpoint.clone(), cx);
                        }
                    });
                }),
            );

            if matches!(&context_entry.endpoint, FileEndpoint::Remote { .. }) {
                let action_entity = context_entity.clone();
                let action_endpoint = context_entry.endpoint.clone();
                let action_directory = context_directory.clone();
                menu = menu.item(
                    PopupMenuItem::new(locale::text("粘贴到此目录", "Paste into this folder"))
                        .icon(Icon::new(AssetIconName::ClipboardPaste))
                        .on_click(move |_, _, app| {
                            action_entity.update(app, |pane, cx| {
                                if pane.browser_action_snapshot_is_current(
                                    side,
                                    context_generation,
                                    &action_directory,
                                ) {
                                    pane.paste_remote_clipboard(action_endpoint.clone(), cx);
                                }
                            });
                        }),
                );
            }
        }

        menu = menu.item(PopupMenuItem::separator());
        if matches!(&context_entry.endpoint, FileEndpoint::Remote { .. }) {
            let action_entry = context_entry.clone();
            let action_entity = context_entity.clone();
            let action_names = Arc::clone(&context_names);
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("复制项目", "Copy item"))
                    .icon(IconName::Copy)
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
                                pane.copy_to_remote_clipboard(side, Some(action_entry.clone()), cx);
                            }
                        });
                    }),
            );
        }

        let action_entity = context_entity.clone();
        let action_endpoint = context_entry.endpoint.clone();
        let action_directory = context_directory.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text("复制路径", "Copy path"))
                .icon(IconName::Copy)
                .on_click(move |_, _, app| {
                    action_entity.update(app, |pane, cx| {
                        if pane.browser_action_snapshot_is_current(
                            side,
                            context_generation,
                            &action_directory,
                        ) {
                            pane.copy_path_to_clipboard(&action_endpoint, cx);
                        }
                    });
                }),
        );

        let action_entry = context_entry.clone();
        let action_entity = context_entity.clone();
        let action_names = Arc::clone(&context_names);
        let action_directory = context_directory.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text("重命名", "Rename"))
                .icon(Icon::new(AssetIconName::Pencil))
                .on_click(move |_, window, app| {
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
                }),
        );

        if can_change_permissions {
            let action_entry = context_entry.clone();
            let action_entity = context_entity.clone();
            let action_names = Arc::clone(&context_names);
            let action_directory = context_directory.clone();
            menu = menu.item(
                PopupMenuItem::new(locale::text("修改权限", "Change permissions"))
                    .icon(Icon::new(AssetIconName::Pencil))
                    .on_click(move |_, window, app| {
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
                                pane.prepare_permissions(side, window, cx);
                            }
                        });
                    }),
            );
        }

        menu = menu.item(PopupMenuItem::separator());
        let action_entry = context_entry.clone();
        let action_entity = context_entity.clone();
        let action_names = Arc::clone(&context_names);
        let action_directory = context_directory.clone();
        menu = menu.item(
            PopupMenuItem::new(locale::text(
                if context_entry.kind == FileEntryKind::Directory {
                    "删除目录"
                } else {
                    "删除"
                },
                if context_entry.kind == FileEntryKind::Directory {
                    "Delete folder"
                } else {
                    "Delete"
                },
            ))
            .icon(Icon::new(AssetIconName::Trash))
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
                        pane.delete_selected(cx);
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
    let already_selected = pane
        .panels
        .get(side)
        .is_some_and(|panel| panel.selection.contains(&entry.name));
    pane.activate_side(side, cx);
    if already_selected {
        return true;
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

/// 多选右键操作要求菜单打开时的 endpoint 集合仍一致，避免异步重绘后误删或误下另一组文件。
///
/// @author kongweiguang
fn select_context_selection(
    pane: &mut FilesPane,
    side: usize,
    expected: &[FileEntry],
    generation: u64,
    directory: &FileEndpoint,
    cx: &mut Context<FilesPane>,
) -> bool {
    if !pane.browser_action_snapshot_is_current(side, generation, directory) {
        return false;
    }
    let current = pane.selected_entries(side);
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

/// 紧凑右栏采用 main 的本机时区及当前年格式；中央工作区保留既有 UTC 值。
///
/// @author kongweiguang
fn format_file_modified(entry: &FileEntry, compact_layout: bool) -> String {
    file_modified_timestamp(entry)
        .map(|timestamp| {
            if compact_layout {
                let local = timestamp.with_timezone(&Local);
                if local.year() == Local::now().year() {
                    local.format("%m-%d %H:%M").to_string()
                } else {
                    local.format("%Y-%m-%d").to_string()
                }
            } else {
                timestamp.format("%m-%d %H:%M").to_string()
            }
        })
        .unwrap_or_else(|| "—".to_owned())
}

/// 紧凑右栏悬浮详情使用完整本地时间；中央工作区保留显式 UTC 标记。
///
/// @author kongweiguang
fn format_file_modified_tooltip(entry: &FileEntry, compact_layout: bool) -> String {
    file_modified_timestamp(entry)
        .map(|timestamp| {
            if compact_layout {
                timestamp
                    .with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            } else {
                timestamp.format("%Y-%m-%d %H:%M UTC").to_string()
            }
        })
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
    action: Option<AnyElement>,
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
        .when_some(action, |state, action| {
            state.child(
                div()
                    .w_full()
                    .px_3()
                    .pt_2()
                    .flex()
                    .justify_end()
                    .child(action),
            )
        })
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

#[cfg(test)]
#[path = "../../../../tests/unit/files_context_menu_regressions.rs"]
mod context_menu_regression_tests;
