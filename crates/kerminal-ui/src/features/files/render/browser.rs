//! 文件双栏面板的路径栏、工具栏和目录列表。该模块只投影现有 FileService 状态。
//!
//! @author kongweiguang

use chrono::{DateTime, Datelike, Local, Utc};
use gpui_kit::{
    AnyElement, App, ClickEvent, Context, ExternalPaths, InteractiveElement as _, IntoElement,
    KeyDownEvent, Pixels, SharedString, StatefulInteractiveElement as _, Window,
    base::v_virtual_list,
    canvas,
    component::{
        ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
        button::{Button, ButtonCustomVariant, ButtonVariants as _},
        input::{Enter, Escape, Input, InputGroup, InputGroupAddon, InputGroupText},
        menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem},
        scroll::ScrollableElement as _,
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px, relative, size,
};
use kerminal_application::{AppError, AppResult};
use kerminal_domain::files::{ConflictPolicy, FileEndpoint, FileEntry, FileEntryKind, RemotePath};
use std::{cmp::Ordering, path::PathBuf, rc::Rc, sync::Arc};

use crate::{locale, theme::ThemeTokens};
use gpui_kit::assets::IconName as AssetIconName;

use super::super::context_menu::{FileMenuSnapshot, FileMenuTarget, build_file_context_menu};
#[cfg(test)]
use super::super::context_menu::{select_context_entry, select_context_selection};
use super::super::drag_drop::{
    DropDirectorySnapshot, DropRejection, FileDragPreview, FileTransferDrag, copy_target_rejection,
    drop_rejection_message, external_drop_directory_ready, external_paths_drop_is_allowed,
    internal_drop_is_allowed,
};
use super::super::{FileListNavigation, FileSort, FileSortColumn};
use super::{FilesPane, asset_icon_button, icon_button};

/// 单个目录面板的动作视图快照，宽窄布局共用同一份选择、目标和测量后的工具栏状态。
///
/// @author kongweiguang
#[derive(Clone, Copy)]
struct FileActionsView<'a> {
    side: usize,
    copy_target_available: bool,
    copy_target_rejection: Option<DropRejection>,
    transfer_submission_busy: bool,
    loading: bool,
    conflict_policy: ConflictPolicy,
    delete_confirmation: bool,
    delete_action_busy: bool,
    endpoint: &'a FileEndpoint,
    selected: Option<&'a FileEntry>,
    selected_count: usize,
    has_transferable_selection: bool,
    has_remote_selection: bool,
    visible_count: usize,
    total_entry_count: usize,
    file_count: usize,
    directory_count: usize,
    entry_filter: super::super::view_mode::FileEntryFilter,
    compact_layout: bool,
    show_hidden_files: bool,
    view_mode: super::super::view_mode::FilesViewMode,
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

/// 表头与目录行共享断点：极窄时优先名称，320px 起显示时间，400px 起显示大小，540px 起显示权限。
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
    show_scheme: bool,
    show_terminal_follow: bool,
    bookmarks: Vec<super::super::FileBookmark>,
    bookmarks_loading: bool,
    bookmark_error: Option<String>,
    current_path_bookmarkable: bool,
    current_path_is_bookmarked: bool,
    follow_terminal_directory: bool,
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
    /// 极窄面板隐藏时间以保留文件名，较宽时沿用 main 的大小和权限列断点。
    ///
    /// @author kongweiguang
    fn for_width(width: Pixels) -> Self {
        Self {
            size: width >= px(400.),
            modified: width >= px(320.),
            permissions: width >= px(540.),
        }
    }
}

impl FileBrowserLayout {
    /// 只按面板宽度投影元数据列；动作条固定为同一组高频按钮，不随宽度换组。
    ///
    /// @author kongweiguang
    fn for_width(width: Pixels) -> Self {
        Self {
            columns: FileTableColumns::for_width(width),
        }
    }
}

/// 视图、类型筛选和窄栏次要动作共享菜单，保证目录栏缩窄时命令仍可访问。
/// 在途删除只禁用对应菜单动作，保留导航、刷新与其它工具的可用性。
///
/// @author kongweiguang
fn render_file_more_menu(
    view: FileActionsView<'_>,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> impl IntoElement {
    use super::super::view_mode::{FileEntryFilter, FilesViewMode};

    let FileActionsView {
        side,
        view_mode,
        conflict_policy,
        selected,
        selected_count,
        entry_filter,
        total_entry_count,
        file_count,
        directory_count,
        delete_confirmation,
        delete_action_busy,
        endpoint,
        has_remote_selection,
        transfer_submission_busy,
        ..
    } = view;
    let has_file =
        selected_count == 1 && selected.is_some_and(|entry| entry.kind == FileEntryKind::File);
    let has_selection = selected_count > 0;
    let can_upload = !matches!(endpoint, FileEndpoint::Local { .. });
    let entity = cx.entity().clone();
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
    ];
    let policies = [
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
    ];
    compact_toolbar_button(
        Button::new(format!("files-more-menu-{side}")).compact(),
        theme,
        false,
    )
    .w(px(32.))
    .h(px(32.))
    .flex_shrink_0()
    .debug_selector(move || format!("rr-files-more-menu-{side}"))
    .icon(IconName::Ellipsis)
    .tooltip(locale::text("更多文件操作", "More file actions"))
    .accessibility_label(locale::text("更多文件操作", "More file actions"))
    .dropdown_menu_with_anchor(
        gpui_kit::Anchor::TopRight,
        move |mut menu, window, menu_cx| {
            menu = crate::ui_popup::dropdown_menu(menu, theme);
            menu = menu
                .item(PopupMenuItem::label(locale::text("文件视图", "File view")).disabled(true));
            for (target, icon, label) in &modes {
                let target = *target;
                let icon = *icon;
                let entity = entity.clone();
                menu = menu.item(
                    PopupMenuItem::new(label.clone())
                        .icon(icon)
                        .checked(view_mode == target)
                        .on_click(move |_, _, app| {
                            entity.update(app, |pane, cx| {
                                pane.activate_side(side, cx);
                                pane.set_file_view_mode(target, cx);
                            });
                        }),
                );
            }
            if view_mode != FilesViewMode::Tree {
                let filter_name = match entry_filter {
                    FileEntryFilter::All => locale::text("全部", "All"),
                    FileEntryFilter::Files => locale::text("文件", "Files"),
                    FileEntryFilter::Directories => locale::text("目录", "Folders"),
                };
                let filter_label = locale::text(
                    format!("文件类型：{filter_name}"),
                    format!("File type: {filter_name}"),
                );
                let filter_entity = entity.clone();
                menu =
                    menu.separator()
                        .submenu(filter_label, window, menu_cx, move |menu, _, _| {
                            let mut menu = crate::ui_popup::dropdown_menu(menu, theme);
                            for (filter, label) in [
                                (
                                    FileEntryFilter::All,
                                    locale::text(
                                        format!("全部 {total_entry_count}"),
                                        format!("All {total_entry_count}"),
                                    ),
                                ),
                                (
                                    FileEntryFilter::Files,
                                    locale::text(
                                        format!("文件 {file_count}"),
                                        format!("Files {file_count}"),
                                    ),
                                ),
                                (
                                    FileEntryFilter::Directories,
                                    locale::text(
                                        format!("目录 {directory_count}"),
                                        format!("Folders {directory_count}"),
                                    ),
                                ),
                            ] {
                                let filter_entity = filter_entity.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(label)
                                        .checked(entry_filter == filter)
                                        .on_click(move |_, _, app| {
                                            filter_entity.update(app, |pane, cx| {
                                                pane.set_file_entry_filter(side, filter, cx)
                                            });
                                        }),
                                );
                            }
                            menu
                        });
            }
            menu = menu.separator();
            let action = entity.clone();
            menu = menu.item(
                PopupMenuItem::new(if delete_action_busy {
                    locale::text("删除中…", "Deleting…")
                } else if delete_confirmation {
                    locale::text("确认删除所选项目", "Confirm deletion")
                } else {
                    locale::text("删除所选项目", "Delete selected items")
                })
                .danger()
                .icon(Icon::new(AssetIconName::Trash))
                .disabled(delete_action_busy || !has_selection)
                .on_click(move |_, _, app| {
                    action.update(app, |pane, cx| {
                        pane.activate_side(side, cx);
                        pane.delete_selected(cx);
                    })
                }),
            );
            if can_upload {
                let action = entity.clone();
                menu = menu.item(
                    PopupMenuItem::new(locale::text("下载所选项目", "Download selected items"))
                        .icon(Icon::new(AssetIconName::Download))
                        .disabled(transfer_submission_busy || !has_remote_selection)
                        .on_click(move |_, _, app| {
                            action.update(app, |pane, cx| {
                                pane.activate_side(side, cx);
                                pane.prompt_download(cx);
                            })
                        }),
                );
            }
            menu = menu.item(PopupMenuItem::separator()).item(
                PopupMenuItem::label(locale::text(
                    "以下设置只影响之后加入的任务",
                    "These settings apply only to newly queued tasks",
                ))
                .disabled(true),
            );
            for (policy, label) in &policies {
                let policy = *policy;
                let entity = entity.clone();
                menu = menu.item(
                    PopupMenuItem::new(label.clone())
                        .checked(conflict_policy == policy)
                        .on_click(move |_, _, app| {
                            entity.update(app, |pane, cx| pane.set_conflict_policy(policy, cx));
                        }),
                );
            }
            let open_entity = entity.clone();
            menu = menu.item(PopupMenuItem::separator()).item(
                PopupMenuItem::new(locale::text("在编辑器中打开", "Open in editor"))
                    .icon(IconName::FileText)
                    .disabled(!has_file)
                    .on_click(move |_, _, app| {
                        open_entity.update(app, |pane, cx| {
                            pane.activate_side(side, cx);
                            if let Some(entry) = pane.panels[side].selected.as_ref()
                                && entry.kind == FileEntryKind::File
                            {
                                let endpoint = entry.endpoint.clone();
                                pane.request_open_file(endpoint, cx);
                            }
                        });
                    }),
            );
            menu
        },
    )
    .offset(px(6.))
}

/// 路径和操作统一为两排；紧凑模式仅显示当前终端目标，工具栏和列按当前面板实际 bounds 分档。
/// 紧凑右栏复用 rail 自身外壳，独立边框只属于文件列表卡；中央工作区继续用 endpoint 边界显示活动面板。
/// 键盘焦点留在列表 owner；实际行、虚拟尺寸和翻页共用当前帧的缩放行高。
/// 同目录刷新保留已提交清单；导航到其它目录时不冒用旧清单，忙态只由原刷新图标表达。
/// 删除忙态仅投影到对应菜单，避免阻止用户在后台删除时继续浏览。
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
    let shared_host_picker = pane.host_picker.enabled && !pane.compact_layout;
    let host_picker_header = shared_host_picker
        .then(|| super::host_picker::render_selector_header(pane, side, theme, window, cx));
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
    let has_transferable_selection = selected_entries
        .iter()
        .any(super::super::context_menu::is_transferable);
    let has_remote_selection = selected_entries.iter().any(|entry| {
        !matches!(&entry.endpoint, FileEndpoint::Local { .. })
            && super::super::context_menu::is_transferable(entry)
    });
    let panel = &pane.panels[side];
    let endpoint = panel.endpoint.clone();
    let copy_target_rejection = copy_target_rejection(
        selected_entries.iter(),
        &endpoint,
        &pane.panels[1 - side].endpoint,
    );
    let toolbar_projection =
        FileBrowserToolbarProjection::for_endpoint(pane.compact_layout, &endpoint);
    let active = pane.active_side == side;
    let locked = side == 0 && pane.locked_left_host_id.is_some();
    let selected = panel.selected.clone();
    let panel_generation = panel.generation;
    let directory_ready = !pane.host_picker.target_pending[side]
        && external_drop_directory_ready(
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
    let listing = panel
        .listing
        .as_ref()
        .filter(|listing| {
            &listing.directory == panel.requested_endpoint.as_ref().unwrap_or(&endpoint)
        })
        .cloned();
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
        .debug_selector(move || format!("rr-files-browser-{side}"))
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
    let source_loading_for_drop =
        source_panel.loading || pane.host_picker.target_pending[source_side_for_drop];
    let source_listing_directory_for_drop = source_panel
        .listing
        .as_ref()
        .map(|listing| listing.directory.clone());
    let source_endpoint_for_validation = source_endpoint_for_drop.clone();
    let source_listing_for_validation = source_listing_directory_for_drop.clone();
    let source_endpoint_for_feedback = source_endpoint_for_drop;
    let source_listing_for_feedback = source_listing_directory_for_drop;
    let pane_id_for_drop = cx.entity_id().as_u64();
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
                    pane_id_for_drop,
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
                pane_id_for_drop,
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
            show_scheme: !pane.compact_layout || columns.size,
            show_terminal_follow: toolbar_projection.show_terminal_follow
                && pane.current_terminal_binding.is_some(),
            bookmarks: path_bookmarks,
            bookmarks_loading,
            bookmark_error,
            current_path_bookmarkable,
            current_path_is_bookmarked,
            follow_terminal_directory: pane.follow_terminal_directory,
            theme,
        },
        cx,
    )
    .into_any_element();
    let actions_bar = render_file_actions(
        FileActionsView {
            side,
            copy_target_available: pane.can_transfer_selected_to_other_side(side),
            copy_target_rejection,
            transfer_submission_busy: pane.transfer_submission_busy,
            conflict_policy,
            delete_confirmation,
            delete_action_busy: pane.delete_action_busy,
            endpoint: &endpoint,
            selected: selected.as_ref(),
            selected_count,
            has_transferable_selection,
            has_remote_selection,
            visible_count,
            total_entry_count: all_count,
            file_count,
            directory_count,
            entry_filter: panel.entry_filter,
            compact_layout: pane.compact_layout,
            show_hidden_files,
            view_mode: pane.view_mode,
            loading,
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

    // 根目录读取失败时树没有可展示节点；沿用列表内联错误卡，保持同一错误文案与唯一重试入口。
    if tree_mode && (listing.is_some() || panel.directory_error.is_none()) {
        let background_entity = cx.entity().clone();
        let background_snapshot = FileMenuSnapshot {
            side,
            generation: panel_generation,
            directory: endpoint.clone(),
            show_hidden: show_hidden_files,
            target: FileMenuTarget::Background,
        };
        let tree = div()
            .id(format!("files-browser-tree-{side}"))
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(super::tree::render_tree_content(
                pane,
                side,
                loading,
                listing.as_ref(),
                show_hidden_files,
                theme,
                cx,
            ))
            .context_menu(move |menu, _, app| {
                build_file_context_menu(
                    menu,
                    theme,
                    background_entity.clone(),
                    background_snapshot.clone(),
                    app,
                )
            });
        return panel_view.child(table_heading).child(tree);
    }

    let keyboard_names = Arc::clone(&ordered_names);
    let keyboard_directory = endpoint.clone();
    let keyboard_generation = panel_generation;
    let row_height = pane.file_row_height * window.rem_size().as_f32() / 16.;
    let list_debug_selector = if pane.compact_layout {
        format!("files-browser-list-{side}")
    } else {
        format!("central-files-browser-list-{side}")
    };
    let mut list = div()
        .id(format!("files-browser-list-{side}"))
        .debug_selector(move || list_debug_selector.clone())
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
                    FileListNavigation {
                        side,
                        generation: keyboard_generation,
                        directory: &keyboard_directory,
                        ordered_names: keyboard_names.as_slice(),
                        row_height,
                    },
                    event,
                    cx,
                );
                cx.stop_propagation();
            } else if key == "enter" {
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

    let trust_request = pane.current_host_key_trust_request();
    let host_key_trust_busy = trust_request.as_ref().is_some_and(|request| {
        endpoint.host_id() == Some(&request.host_id)
            && matches!(&endpoint, FileEndpoint::Remote { .. })
    });
    if loading && listing.is_none() {
        // 首次读取没有可信内容时保持列表框架，避免重复加载文案与工具栏争夺空间。
        list = list.child(div().flex_1());
    } else if listing.is_none() {
        // 错误卡是这一侧唯一的失败说明，也承载原位恢复动作；信任只属于真实主机密钥请求，
        // 不能用“读取失败”推断用户需要写入 known_hosts。重试始终命中失败目录本身。
        let can_retry = panel.directory_error.is_some() && panel.failed_directory.is_some();
        let can_trust = side == 1
            && panel.directory_error.is_some()
            && matches!(&endpoint, FileEndpoint::Remote { .. })
            && trust_request
                .as_ref()
                .is_some_and(|request| endpoint.host_id() == Some(&request.host_id));
        let retry_button = can_retry.then(|| {
            Button::new(format!("files-directory-error-retry-{side}"))
                .compact()
                .ghost()
                .icon(IconName::RefreshCw)
                .label(locale::text("重试", "Retry"))
                .accessibility_label(locale::text("重新读取当前目录", "Retry current directory"))
                .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                    pane.retry_panel_load(side, cx);
                }))
                .into_any_element()
        });
        let trust_button = can_trust.then(|| {
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
        let actions = (retry_button.is_some() || trust_button.is_some()).then(|| {
            div()
                .flex()
                .flex_wrap()
                .justify_end()
                .gap_2()
                .children(retry_button)
                .children(trust_button)
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
            actions,
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
    let background_snapshot = FileMenuSnapshot {
        side,
        generation: panel_generation,
        directory: endpoint.clone(),
        show_hidden: show_hidden_files,
        target: FileMenuTarget::Background,
    };
    let list = list.context_menu(move |menu, _, app| {
        build_file_context_menu(
            menu,
            theme,
            background_entity.clone(),
            background_snapshot.clone(),
            app,
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

/// 地址、跳转和书签保持同一条导航带；只有输入区域收缩，固定控件不会换行或挤成两排。
/// 按钮和 Enter 共用导航校验，窄栏仍可通过编辑与完整路径提示访问长路径。
/// 书签空态保留保存路径的提示，使图标操作在无数据时也可发现。
/// 路径栏只保留目录跟随开关并暴露其开关状态；Shell 上报由会话集成负责，不在此写入远端启动文件。
/// InputGroup 独占地址边框和焦点态；组合输入优先消费 Enter/Escape，避免误提交或清空路径。
///
/// @author kongweiguang
fn render_path_bar(view: FilePathBarView<'_>, cx: &mut Context<FilesPane>) -> impl IntoElement {
    let FilePathBarView {
        endpoint,
        path_input,
        side,
        compact_layout,
        show_scheme,
        show_terminal_follow,
        bookmarks,
        bookmarks_loading,
        bookmark_error,
        current_path_bookmarkable,
        current_path_is_bookmarked,
        follow_terminal_directory,
        theme,
    } = view;
    let scheme = match endpoint {
        FileEndpoint::Local { .. } => "file://",
        FileEndpoint::Remote { .. } => "sftp://",
        FileEndpoint::Container { .. } => "container://",
    };
    let full_path = endpoint_path(endpoint);
    let system_font_family = cx.theme().font_family.clone();
    let enter_input = path_input.clone();
    let escape_input = path_input.clone();
    let key_input = path_input.clone();
    let path_field = div()
        .id(format!("files-path-keyboard-{side}"))
        .debug_selector(move || format!("rr-files-path-input-{side}"))
        .min_w_0()
        .h(theme.metrics.control_height)
        .flex()
        .items_center()
        .tooltip(move |window, cx| Tooltip::new(full_path.clone()).build(window, cx))
        .capture_action(move |_: &Enter, _, cx| {
            if enter_input.read(cx).is_composing() {
                cx.stop_propagation();
            }
        })
        .capture_action(move |_: &Escape, window, cx| {
            crate::features::input_composition::cancel_input_composition(&escape_input, window, cx);
        })
        .on_key_down(cx.listener(move |pane, event: &KeyDownEvent, window, cx| {
            if key_input.read(cx).is_composing() {
                return;
            }
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
            InputGroup::new(format!("files-path-{side}"))
                .small()
                .h_full()
                .rounded_md()
                .border_color(theme.border)
                .bg(theme.background)
                .input(
                    Input::new(path_input)
                        .w_full()
                        .h_full()
                        .text_size(theme.text_sm)
                        .when(compact_layout, |input| {
                            input.font_family(system_font_family)
                        })
                        .aria_label(locale::text("目录路径", "Directory path")),
                )
                .when(show_scheme, |group| {
                    group.addon(
                        InputGroupAddon::new(format!("files-path-scheme-{side}"))
                            .child(InputGroupText::new().text_color(theme.accent).child(scheme)),
                    )
                }),
        );

    let jump = Button::new(format!("files-path-jump-{side}"))
        .compact()
        .w_8()
        .when(compact_layout, |button| button.w_6())
        .h(theme.metrics.control_height)
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
        .w_8()
        .when(compact_layout, |button| button.w_6())
        .h(theme.metrics.control_height)
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
        .w(theme.metrics.icon_size + theme.metrics.gap)
        .h(theme.metrics.control_height)
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
                            PopupMenuItem::element(move |_, cx| {
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
                                            .custom(
                                                ButtonCustomVariant::new(cx)
                                                    .foreground(theme.error)
                                                    .hover(theme.error.opacity(0.1))
                                                    .active(theme.error.opacity(0.16)),
                                            )
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
            .w_8()
            .when(compact_layout, |button| button.w_6())
            .h(theme.metrics.control_height)
            .outline()
            .when(follow_terminal_directory, Button::primary)
            .when(!follow_terminal_directory, Button::ghost)
            .toggled(follow_terminal_directory)
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
    let bookmark_control = div()
        .h(theme.metrics.control_height)
        .flex()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .child(bookmark_toggle)
        .child(div().w(px(1.)).h(px(20.)).bg(theme.border))
        .child(bookmark_menu_button);

    let path_controls = div()
        .w_full()
        .min_w_0()
        .min_h(theme.metrics.toolbar_height)
        .flex()
        .flex_nowrap()
        .items_center()
        .gap_1()
        .when(compact_layout, |bar| {
            bar.px_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.tool_panel_surface)
        })
        .child(path_field.flex_1())
        .child(jump)
        .child(bookmark_control.flex_shrink_0())
        .when_some(follow, |bar, follow| bar.child(follow.flex_shrink_0()));

    div()
        .id(format!("files-path-bar-{side}"))
        .w_full()
        .min_h(theme.metrics.toolbar_height + theme.metrics.gap)
        .flex_shrink_0()
        .px(if compact_layout {
            theme.metrics.gap
        } else {
            theme.metrics.panel_padding
        })
        .flex()
        .items_center()
        .when(!compact_layout, |bar| {
            bar.border_b_1().border_color(theme.border)
        })
        .bg(theme.panel)
        .child(path_controls)
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

/// 宽窄工作台共用上级、刷新、隐藏、上传、新建、传输、计数和更多动作。
/// 紧凑动作槽从同一密度投影计算，刷新时保留原有命中区；窄栏放大字号时换行以保留所有操作。
/// 两种布局始终只显示当前投影项数；类型、树展开范围和已选数在悬停说明中明确区分。
///
/// @author kongweiguang
fn render_file_actions(
    view: FileActionsView<'_>,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    let FileActionsView {
        side,
        copy_target_available,
        copy_target_rejection,
        transfer_submission_busy,
        loading,
        endpoint,
        selected_count,
        has_transferable_selection,
        has_remote_selection,
        visible_count,
        entry_filter,
        compact_layout,
        show_hidden_files,
        view_mode,
        ..
    } = view;
    let can_upload = !matches!(endpoint, FileEndpoint::Local { .. });

    let more_view = view;
    let more: AnyElement = if !compact_layout {
        render_file_more_menu(more_view, theme, cx).into_any_element()
    } else {
        let mode_entity = cx.entity().clone();
        compact_toolbar_button(
            Button::new(format!("files-view-mode-{side}")).compact(),
            theme,
            false,
        )
        .size(theme.metrics.control_height)
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
            ] {
                let action_entity = mode_entity.clone();
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .icon(match mode {
                            super::super::view_mode::FilesViewMode::List => AssetIconName::List,
                            super::super::view_mode::FilesViewMode::Tree => AssetIconName::ListTree,
                        })
                        .checked(view_mode == mode)
                        .on_click(move |_, _, app| {
                            action_entity.update(app, |pane, cx| pane.set_file_view_mode(mode, cx));
                        }),
                );
            }
            menu
        })
        .offset(px(6.))
        .into_any_element()
    };

    let parent_disabled = super::super::parent_endpoint(endpoint).is_none();
    let parent = icon_button(
        format!("files-parent-{side}"),
        IconName::ArrowUp,
        locale::text("上级目录", "Parent directory"),
        parent_disabled,
    );
    let parent = compact_toolbar_button(parent, theme, parent_disabled)
        .size(theme.metrics.control_height)
        .flex_shrink_0()
        .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| pane.navigate_parent(side, cx)));
    let refresh_disabled = loading;
    let refresh = icon_button(
        format!("files-refresh-{side}"),
        IconName::RefreshCw,
        locale::text("刷新目录", "Refresh directory"),
        refresh_disabled,
    );
    let refresh = compact_toolbar_button(refresh, theme, false)
        .size(theme.metrics.control_height)
        .flex_shrink_0()
        .loading(refresh_disabled)
        .loading_icon(IconName::RefreshCw)
        .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| pane.refresh_panel(side, cx)));

    let count_scope = if view_mode == super::super::view_mode::FilesViewMode::Tree {
        locale::text("展开项数：", "Expanded items: ")
    } else {
        match entry_filter {
            super::super::view_mode::FileEntryFilter::All => {
                locale::text("当前显示项数：", "Visible items: ")
            }
            super::super::view_mode::FileEntryFilter::Files => {
                locale::text("显示文件数：", "Visible files: ")
            }
            super::super::view_mode::FileEntryFilter::Directories => {
                locale::text("显示目录数：", "Visible folders: ")
            }
        }
    };
    let count_tooltip: SharedString = if selected_count == 0 {
        format!("{count_scope}{visible_count}").into()
    } else {
        let selection_scope = locale::text(" · 已选项数：", " · Selected items: ");
        format!("{count_scope}{visible_count}{selection_scope}{selected_count}").into()
    };
    let count_label: SharedString = visible_count.to_string().into();

    let mut actions = div()
        .id(format!("files-actions-{side}"))
        .debug_selector(move || format!("rr-files-actions-{side}"))
        .w_full()
        .min_h(theme.metrics.toolbar_height)
        .flex_shrink_0()
        .px(theme.metrics.gap)
        .py(theme.metrics.gap / 2.)
        .flex()
        .flex_wrap()
        .items_center()
        .gap(theme.metrics.gap / 4.)
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
            .size(theme.metrics.control_height)
            .flex_shrink_0()
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
            .size(theme.metrics.control_height)
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
        .size(theme.metrics.control_height)
        .flex_shrink_0()
        .debug_selector(move || format!("files-new-folder-{side}"))
        .on_click(cx.listener(move |pane, _: &ClickEvent, window, cx| {
            pane.activate_side(side, cx);
            pane.prepare_create_directory(window, cx);
        })),
    );
    if compact_layout {
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
                .size(theme.metrics.control_height)
                .flex_shrink_0()
                .debug_selector(move || format!("files-download-{side}"))
                .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                    pane.activate_side(side, cx);
                    pane.prompt_download(cx);
                })),
            );
        }
    } else {
        let copy_label = if !copy_target_available {
            locale::text("对侧目录尚未就绪", "The opposite directory is not ready")
        } else if let Some(reason) = copy_target_rejection {
            drop_rejection_message(reason).into()
        } else if has_transferable_selection {
            if side == 0 {
                locale::text("复制到右侧", "Copy to right panel")
            } else {
                locale::text("复制到左侧", "Copy to left panel")
            }
        } else {
            locale::text("请先选择要复制的项目", "Select an item to copy")
        };
        let copy_disabled = !copy_target_available
            || copy_target_rejection.is_some()
            || !has_transferable_selection
            || transfer_submission_busy;
        actions = actions.child(
            div()
                .id(format!("files-transfer-divider-{side}"))
                .debug_selector(move || format!("files-transfer-divider-{side}"))
                .w(px(1.))
                .h(px(16.))
                .flex_shrink_0()
                .bg(theme.border),
        );
        actions = actions.child(
            compact_toolbar_button(
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
                    copy_disabled,
                ),
                theme,
                copy_disabled,
            )
            .size(theme.metrics.control_height)
            .flex_shrink_0()
            .debug_selector(move || format!("files-copy-to-other-{side}"))
            .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                pane.activate_side(side, cx);
                pane.transfer_selected(cx);
            })),
        );
    }
    // 计数沿用左栏分组徽标的胶囊样式：使用固定最小尺寸而非随文本伸缩，
    // 目录项数变化时不会带动右侧“更多”入口位移，超长计数仍按上限省略。
    let count_pill = div()
        .id(format!("files-toolbar-count-{side}"))
        .debug_selector(move || format!("files-toolbar-count-{side}"))
        .min_w(px(22.))
        .h(px(22.))
        .mx(px(2.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .px_1()
        .rounded_full()
        .bg(theme.secondary)
        .text_center()
        .text_color(theme.muted)
        .text_size(theme.text_xs)
        .max_w(theme.metrics.control_height * 5.)
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .aria_label(count_tooltip.clone())
        .tooltip(move |window, cx| Tooltip::new(count_tooltip.clone()).build(window, cx))
        .child(count_label);

    actions
        .child(div().flex_1().min_w(px(0.)))
        .child(count_pill)
        .child(more)
        .into_any_element()
}

/// 上传菜单行使用共享字体和行高，同时沿用 PopupMenu 的点击、键盘和焦点行为。
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
            .h(theme.metrics.row_height)
            .flex()
            .flex_row()
            .items_center()
            .gap(theme.metrics.gap)
            .rounded(theme.control_radius)
            .text_size(theme.text_sm)
            .line_height(relative(1.25))
            .text_color(theme.foreground)
            .aria_label(accessible_label.clone())
            .child(
                Icon::new(icon)
                    .size(theme.metrics.icon_size)
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
/// 实际行与虚拟列表共用同一密度行高，防止滚动几何错位；右键仅保留直接作用于文件的常用命令。
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
    let context_entity = cx.entity().clone();
    let context_snapshot = FileMenuSnapshot {
        side,
        generation: source_generation,
        directory: source_directory.clone(),
        show_hidden: show_hidden_files,
        target: FileMenuTarget::List {
            entry: entry.clone(),
            selected: Arc::clone(&selected_drag_entries),
            row_selected: selected,
            ordered_names: Arc::clone(&ordered_names),
        },
    };
    let accessible_name = format!("{}, {}", entry.name, accessible_kind);
    let row_selector = format!("files-row-{side}-{name}");
    let row_debug_selector = if compact_layout {
        row_selector.clone()
    } else {
        format!("central-files-row-{side}-{name}")
    };
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
                cx.entity_id().as_u64(),
                side,
                source_directory,
                source_generation,
                Arc::clone(&selected_drag_entries),
                0,
            )
        } else {
            FileTransferDrag::single(
                cx.entity_id().as_u64(),
                side,
                source_directory,
                source_generation,
                entry,
            )
        };
        row = row.on_drag(drag_payload, move |payload, cursor_offset, _, app| {
            let count = payload.selected_count();
            app.new(move |_| FileDragPreview::new(count, cursor_offset, theme))
        });
        #[cfg(target_os = "windows")]
        {
            let drag_entity = cx.entity().clone();
            row = row.external_drag_payload(move |drag: &FileTransferDrag, _, app| {
                drag_entity.update(app, |pane, cx| pane.external_drag_payload(drag, cx))
            });
        }
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
    row.context_menu(move |menu, _, app| {
        build_file_context_menu(
            menu,
            theme,
            context_entity.clone(),
            context_snapshot.clone(),
            app,
        )
    })
    .into_any_element()
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
/// 引导短句限定在面板宽度内并居中，窄栏不因长句留下独立标点或越过列表边界。
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
                .w_full()
                .min_w(px(0.))
                .px_2()
                .text_center()
                .text_size(theme.text_xs)
                .text_color(theme.subtle)
                .child(if hidden_only {
                    locale::text(
                        "此目录包含隐藏项目。",
                        "This directory contains hidden items.",
                    )
                } else {
                    locale::text(
                        "新建文件夹或复制文件到这里",
                        "Create a folder or copy files here.",
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
