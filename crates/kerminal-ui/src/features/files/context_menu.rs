//! 文件列表、树和空白区共用的菜单策略与冻结目标分发。
//!
//! @author kongweiguang

use std::sync::Arc;

use gpui_kit::{
    App, Context, Entity, SharedString, Window,
    assets::IconName as AssetIconName,
    component::{
        Icon, IconName,
        menu::{PopupMenu, PopupMenuItem},
    },
};
use kerminal_domain::files::{FileEndpoint, FileEntry, FileEntryKind};

use crate::{locale, theme::ThemeTokens};

use super::{
    FilesPane, TransferTargetSnapshot,
    drag_drop::{
        DropRejection, copy_selection_roots, copy_target_rejection, drop_rejection_message,
    },
    selection::SelectionGesture,
    view_mode::FilesViewMode,
};

/// 菜单保留右键发生时的选择与行身份，避免动作被之后的焦点或树刷新重定向。
#[derive(Clone)]
pub(super) enum FileMenuTarget {
    Background,
    List {
        entry: FileEntry,
        selected: Arc<Vec<FileEntry>>,
        row_selected: bool,
        ordered_names: Arc<Vec<String>>,
    },
    Tree {
        entry: FileEntry,
        selected: Arc<Vec<FileEntry>>,
        row_selected: bool,
    },
}

/// 一个菜单只属于创建它的浏览面板代次；目标路径不从后续活动面板推断。
#[derive(Clone)]
pub(super) struct FileMenuSnapshot {
    pub(super) side: usize,
    pub(super) generation: u64,
    pub(super) directory: FileEndpoint,
    pub(super) show_hidden: bool,
    pub(super) target: FileMenuTarget,
}

/// 菜单命令独立于行布局，确保同一能力在列表和树中拥有一致条件。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileMenuAction {
    CopyToOther,
    Open,
    Edit,
    Download,
    UploadFiles,
    UploadDirectories,
    NewDirectory,
    Refresh,
    ToggleHidden,
    CopyPath,
    Rename,
    Permissions,
    Delete,
    Separator,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileMenuItem {
    action: FileMenuAction,
    enabled: bool,
}

/// 使用互斥原因解释禁用状态，优先指出所选类型不支持，其次指出当前可恢复的目标状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CopyUnavailableReason {
    UnsupportedItems,
    Busy,
    SourceUnselected,
    DestinationUnselected,
    DirectoryNotReady,
    UnsafeTarget(DropRejection),
}

impl CopyUnavailableReason {
    /// 保持原因简短，让只在禁用时出现的说明行适应窄菜单与中英文环境。
    fn message(self) -> SharedString {
        match self {
            Self::UnsupportedItems => locale::text(
                "不支持传输链接或特殊文件",
                "Links and special files cannot be copied",
            ),
            Self::Busy => locale::text("传输正在提交，请稍候", "A transfer is being submitted"),
            Self::SourceUnselected => {
                locale::text("请先选择来源主机", "Select a source host first")
            }
            Self::DestinationUnselected => {
                locale::text("请先选择对侧主机", "Select a host on the opposite side")
            }
            Self::DirectoryNotReady => locale::text(
                "两侧目录未就绪，请检查或刷新",
                "Directories are not ready; check or refresh",
            ),
            Self::UnsafeTarget(reason) => drop_rejection_message(reason).into(),
        }
    }
}

impl FileMenuSnapshot {
    /// 已选行上的右键保留批量语义，未选行只作用于捕获的单行。
    fn is_multi(&self) -> bool {
        match &self.target {
            FileMenuTarget::Background => false,
            FileMenuTarget::List {
                selected,
                row_selected,
                ..
            }
            | FileMenuTarget::Tree {
                selected,
                row_selected,
                ..
            } => *row_selected && selected.len() > 1,
        }
    }

    /// 当前行与批量选择共用同一种条目投影，权限和可传输数量由该投影计算。
    fn entries(&self) -> Vec<&FileEntry> {
        match &self.target {
            FileMenuTarget::Background => Vec::new(),
            FileMenuTarget::List {
                entry, selected, ..
            }
            | FileMenuTarget::Tree {
                entry, selected, ..
            } => {
                if self.is_multi() {
                    selected.iter().collect()
                } else {
                    vec![entry]
                }
            }
        }
    }

    /// 仅单行菜单有明确项目；批量动作不得借用右键行作为隐含目标。
    fn single_entry(&self) -> Option<&FileEntry> {
        if self.is_multi() {
            return None;
        }
        match &self.target {
            FileMenuTarget::Background => None,
            FileMenuTarget::List { entry, .. } | FileMenuTarget::Tree { entry, .. } => Some(entry),
        }
    }
}

/// 与现有传输用例保持一致：不跟随符号链接，也不提交特殊文件。
pub(super) fn is_transferable(entry: &FileEntry) -> bool {
    matches!(entry.kind, FileEntryKind::File | FileEntryKind::Directory)
}

/// 与实际提交门禁使用同一面板状态，避免菜单把忙碌、未选择和不支持的类型混为一谈。
fn copy_unavailable_reason(
    pane: &FilesPane,
    snapshot: &FileMenuSnapshot,
) -> Option<CopyUnavailableReason> {
    if !snapshot
        .entries()
        .iter()
        .any(|entry| is_transferable(entry))
    {
        return Some(CopyUnavailableReason::UnsupportedItems);
    }
    if pane.transfer_submission_busy {
        return Some(CopyUnavailableReason::Busy);
    }
    if pane.host_picker.enabled {
        if pane.host_picker.target_unselected[snapshot.side] {
            return Some(CopyUnavailableReason::SourceUnselected);
        }
        if pane.host_picker.target_unselected[1 - snapshot.side] {
            return Some(CopyUnavailableReason::DestinationUnselected);
        }
    }
    if !pane.can_transfer_selected_to_other_side(snapshot.side) {
        return Some(CopyUnavailableReason::DirectoryNotReady);
    }
    copy_target_rejection(
        snapshot.entries(),
        &snapshot.directory,
        &pane.panels[1 - snapshot.side].endpoint,
    )
    .map(CopyUnavailableReason::UnsafeTarget)
}

/// 对列表、树和空白区生成同一份紧凑命令集，工作台仅增加复制到对侧。
fn file_menu_items(
    snapshot: &FileMenuSnapshot,
    compact: bool,
    can_copy: bool,
    busy: bool,
) -> Vec<FileMenuItem> {
    use FileMenuAction as Action;

    let item = |action, enabled| FileMenuItem { action, enabled };
    let separator = || item(Action::Separator, true);
    let entries = snapshot.entries();
    let transferable = entries.iter().any(|entry| is_transferable(entry));
    let downloadable = entries.iter().any(|entry| {
        is_transferable(entry) && !matches!(&entry.endpoint, FileEndpoint::Local { .. })
    });
    let mut items = Vec::new();

    if matches!(&snapshot.target, FileMenuTarget::Background) {
        if !matches!(&snapshot.directory, FileEndpoint::Local { .. }) {
            items.push(item(Action::UploadFiles, !busy));
            items.push(item(Action::UploadDirectories, !busy));
        }
        items.push(item(Action::NewDirectory, true));
        items.push(separator());
        items.push(item(Action::Refresh, true));
        items.push(item(Action::ToggleHidden, true));
        items.push(item(Action::CopyPath, true));
        return items;
    }

    if !compact {
        items.push(item(Action::CopyToOther, can_copy && transferable && !busy));
    }
    if snapshot.is_multi() {
        items.push(item(Action::Download, downloadable && !busy));
        items.push(separator());
        items.push(item(Action::Refresh, true));
        items.push(item(Action::ToggleHidden, true));
        items.push(separator());
        items.push(item(Action::Delete, true));
        return items;
    }

    let Some(entry) = snapshot.single_entry() else {
        return items;
    };
    if entry.kind == FileEntryKind::Directory {
        items.push(item(Action::Open, true));
    } else {
        items.push(item(Action::Edit, entry.kind == FileEntryKind::File));
    }
    items.push(item(Action::Download, downloadable && !busy));
    if entry.kind == FileEntryKind::Directory
        && !matches!(&entry.endpoint, FileEndpoint::Local { .. })
    {
        items.push(item(Action::UploadFiles, !busy));
        items.push(item(Action::UploadDirectories, !busy));
    }
    items.push(separator());
    items.push(item(Action::CopyPath, true));
    items.push(item(Action::Rename, true));
    if matches!(entry.kind, FileEntryKind::File | FileEntryKind::Directory)
        && entry.permissions.is_some()
        && super::permissions::supports_posix_mode_endpoint(&entry.endpoint)
    {
        items.push(item(Action::Permissions, true));
    }
    items.push(separator());
    items.push(item(Action::Delete, true));
    items
}

/// 渲染菜单时冻结对侧目标；回调仍须在提交前再次核验来源、选择和目标代次。
pub(super) fn build_file_context_menu(
    menu: PopupMenu,
    theme: ThemeTokens,
    entity: Entity<FilesPane>,
    mut snapshot: FileMenuSnapshot,
    app: &mut App,
) -> PopupMenu {
    if !matches!(&snapshot.target, FileMenuTarget::Background) {
        let selection = entity.update(app, |pane, cx| {
            if !select_menu_snapshot(pane, &snapshot, cx) {
                return None;
            }
            let selected = pane.selected_entries(snapshot.side);
            let entry = match &snapshot.target {
                FileMenuTarget::List { entry, .. } | FileMenuTarget::Tree { entry, .. } => entry,
                FileMenuTarget::Background => return None,
            };
            selected
                .iter()
                .any(|candidate| {
                    candidate.endpoint == entry.endpoint && candidate.kind == entry.kind
                })
                .then_some(selected)
        });
        let Some(selection) = selection else {
            return crate::ui_popup::context_menu(menu, theme, None).item(
                PopupMenuItem::new(locale::text(
                    "目录或选择已变化，请重新打开菜单。",
                    "The directory or selection changed. Reopen the menu.",
                ))
                .disabled(true),
            );
        };
        match &mut snapshot.target {
            FileMenuTarget::List {
                selected,
                row_selected,
                ..
            }
            | FileMenuTarget::Tree {
                selected,
                row_selected,
                ..
            } => {
                *selected = Arc::new(selection);
                *row_selected = true;
            }
            FileMenuTarget::Background => {}
        }
    }
    let pane = entity.read(app);
    let target = pane.capture_transfer_target_snapshot(1 - snapshot.side);
    let copy_reason = copy_unavailable_reason(pane, &snapshot);
    let can_copy = copy_reason.is_none() && target.is_some();
    let items = file_menu_items(
        &snapshot,
        pane.compact_layout,
        can_copy,
        pane.transfer_submission_busy,
    );
    let mut menu = crate::ui_popup::context_menu(menu, theme, None);
    for spec in items {
        if spec.action == FileMenuAction::Separator {
            menu = menu.separator();
            continue;
        }
        let action_entity = entity.clone();
        let action_snapshot = snapshot.clone();
        let action_target = target.clone();
        let item = render_file_menu_item(spec, &snapshot, &pane.panels[1 - snapshot.side].endpoint)
            .on_click(move |_, window, app| {
                action_entity.update(app, |pane, cx| {
                    dispatch_file_menu_action(
                        pane,
                        &action_snapshot,
                        action_target.as_ref(),
                        spec.action,
                        window,
                        cx,
                    );
                });
            });
        menu = menu.item(item);
        if spec.action == FileMenuAction::CopyToOther
            && !spec.enabled
            && let Some(reason) = copy_reason
        {
            menu = menu.item(PopupMenuItem::label(reason.message()));
        }
    }
    menu
}

/// 文案和图标从同一命令集合生成，避免两种布局出现名称或危险样式漂移。
fn render_file_menu_item(
    spec: FileMenuItem,
    snapshot: &FileMenuSnapshot,
    destination: &FileEndpoint,
) -> PopupMenuItem {
    use FileMenuAction as Action;

    let entries = snapshot.entries();
    let count = entries.len();
    let transferable = if spec.action == Action::CopyToOther {
        let (roots, included) = copy_selection_roots(
            entries.iter().map(|entry| (**entry).clone()).collect(),
            &snapshot.directory,
            destination,
        );
        included
            + roots
                .iter()
                .filter(|entry| {
                    is_transferable(entry)
                        && copy_target_rejection(
                            std::iter::once(*entry),
                            &snapshot.directory,
                            destination,
                        )
                        .is_none()
                })
                .count()
    } else {
        0
    };
    let downloadable = entries
        .iter()
        .filter(|entry| {
            is_transferable(entry) && !matches!(&entry.endpoint, FileEndpoint::Local { .. })
        })
        .count();
    let directory = snapshot
        .single_entry()
        .is_some_and(|entry| entry.kind == FileEntryKind::Directory);
    let target_side = if snapshot.side == 0 {
        "右侧"
    } else {
        "左侧"
    };
    let target_side_en = if snapshot.side == 0 { "right" } else { "left" };
    let item = match spec.action {
        Action::CopyToOther => {
            let (zh, en) = if snapshot.is_multi() && transferable != count {
                (
                    format!("复制可传输 {transferable} 项到{target_side}"),
                    format!("Copy {transferable} transferable items to {target_side_en}"),
                )
            } else if snapshot.is_multi() {
                (
                    format!("复制选中 {count} 项到{target_side}"),
                    format!("Copy {count} selected items to {target_side_en}"),
                )
            } else {
                (
                    format!("复制到{target_side}"),
                    format!("Copy to {target_side_en} panel"),
                )
            };
            PopupMenuItem::new(locale::text(zh, en)).icon(if snapshot.side == 0 {
                IconName::ArrowRight
            } else {
                IconName::ArrowLeft
            })
        }
        Action::Open => PopupMenuItem::new(locale::text("打开", "Open")).icon(IconName::FolderOpen),
        Action::Edit => PopupMenuItem::new(locale::text("打开编辑器", "Open in editor"))
            .icon(Icon::new(AssetIconName::FileSearch)),
        Action::Download => {
            let (zh, en) = if snapshot.is_multi() && downloadable != count {
                (
                    format!("下载可传输 {downloadable} 项"),
                    format!("Download {downloadable} transferable items"),
                )
            } else if snapshot.is_multi() {
                (
                    format!("下载选中 {count} 项"),
                    format!("Download {count} selected items"),
                )
            } else if directory {
                ("下载文件夹".to_owned(), "Download folder".to_owned())
            } else {
                ("下载".to_owned(), "Download".to_owned())
            };
            PopupMenuItem::new(locale::text(zh, en)).icon(Icon::new(AssetIconName::Download))
        }
        Action::UploadFiles => PopupMenuItem::new(if directory {
            locale::text("上传文件到此目录", "Upload files into this folder")
        } else {
            locale::text("上传文件", "Upload files")
        })
        .icon(Icon::new(AssetIconName::Upload)),
        Action::UploadDirectories => PopupMenuItem::new(if directory {
            locale::text("上传文件夹到此目录", "Upload folders into this folder")
        } else {
            locale::text("上传文件夹", "Upload folder")
        })
        .icon(Icon::new(AssetIconName::FolderPlus)),
        Action::NewDirectory => PopupMenuItem::new(locale::text("新建目录", "New folder"))
            .icon(Icon::new(AssetIconName::FolderPlus)),
        Action::Refresh => PopupMenuItem::new(locale::text("刷新目录", "Refresh directory"))
            .icon(IconName::RefreshCw),
        Action::ToggleHidden => PopupMenuItem::new(if snapshot.show_hidden {
            locale::text("隐藏隐藏文件", "Hide hidden files")
        } else {
            locale::text("显示隐藏文件", "Show hidden files")
        })
        .icon(if snapshot.show_hidden {
            IconName::EyeOff
        } else {
            IconName::Eye
        })
        .checked(snapshot.show_hidden),
        Action::CopyPath => PopupMenuItem::new(if snapshot.single_entry().is_some() {
            locale::text("复制路径", "Copy path")
        } else {
            locale::text("复制当前路径", "Copy current path")
        })
        .icon(IconName::Copy),
        Action::Rename => PopupMenuItem::new(locale::text("重命名", "Rename"))
            .icon(Icon::new(AssetIconName::Pencil)),
        Action::Permissions => PopupMenuItem::new(locale::text("修改权限", "Change permissions"))
            .icon(Icon::new(AssetIconName::Pencil)),
        Action::Delete => {
            let (zh, en) = if snapshot.is_multi() {
                (
                    format!("删除选中 {count} 项"),
                    format!("Delete {count} selected items"),
                )
            } else if directory {
                ("删除目录".to_owned(), "Delete folder".to_owned())
            } else {
                ("删除".to_owned(), "Delete".to_owned())
            };
            PopupMenuItem::new(locale::text(zh, en))
                .danger()
                .icon(Icon::new(AssetIconName::Trash))
        }
        Action::Separator => return PopupMenuItem::separator(),
    };
    item.disabled(!spec.enabled)
}

/// 将菜单动作交回既有文件用例；忙碌时先忽略重复复制，避免旧菜单覆盖选择或反馈。
/// 其余动作仍拒绝过期选区或已切换的对侧目标。
fn dispatch_file_menu_action(
    pane: &mut FilesPane,
    snapshot: &FileMenuSnapshot,
    target: Option<&TransferTargetSnapshot>,
    action: FileMenuAction,
    window: &mut Window,
    cx: &mut Context<FilesPane>,
) {
    use FileMenuAction as Action;

    if action == Action::CopyToOther && pane.transfer_submission_busy {
        return;
    }
    if !pane.browser_action_snapshot_is_current(
        snapshot.side,
        snapshot.generation,
        &snapshot.directory,
    ) {
        pane.status = Some(
            locale::text(
                "来源目录已变化，请重新打开菜单。",
                "The source directory changed. Reopen the menu.",
            )
            .to_string(),
        );
        cx.notify();
        return;
    }
    match action {
        Action::Refresh => {
            pane.refresh_panel(snapshot.side, cx);
            return;
        }
        Action::ToggleHidden => {
            pane.toggle_hidden_files(cx);
            return;
        }
        Action::Separator => return,
        _ => {}
    }
    if !select_menu_snapshot(pane, snapshot, cx) {
        return;
    }
    let entry = snapshot.single_entry();
    match action {
        Action::CopyToOther => {
            if !pane.can_transfer_selected_to_other_side(snapshot.side)
                || !target.is_some_and(|target| pane.transfer_target_is_current(target))
            {
                pane.status = Some(
                    locale::text(
                        "目标目录已变化或尚未就绪，请重新打开菜单。",
                        "The destination changed or is not ready. Reopen the menu.",
                    )
                    .to_string(),
                );
                cx.notify();
                return;
            }
            pane.transfer_selected(cx);
        }
        Action::Open => {
            if let Some(entry) = entry {
                pane.open_entry(snapshot.side, entry.clone(), cx);
            }
        }
        Action::Edit => {
            if let Some(entry) = entry {
                pane.request_open_file(entry.endpoint.clone(), cx);
            }
        }
        Action::Download => pane.prompt_download(cx),
        Action::UploadFiles => pane.prompt_upload_files_to(
            entry
                .map(|entry| entry.endpoint.clone())
                .unwrap_or_else(|| snapshot.directory.clone()),
            cx,
        ),
        Action::UploadDirectories => pane.prompt_upload_directories_to(
            entry
                .map(|entry| entry.endpoint.clone())
                .unwrap_or_else(|| snapshot.directory.clone()),
            cx,
        ),
        Action::NewDirectory => pane.prepare_create_directory(window, cx),
        Action::CopyPath => pane.copy_path_to_clipboard(
            entry
                .map(|entry| &entry.endpoint)
                .unwrap_or(&snapshot.directory),
            cx,
        ),
        Action::Rename => pane.prepare_rename(window, cx),
        Action::Permissions => pane.prepare_permissions(snapshot.side, window, cx),
        Action::Delete => pane.delete_selected(cx),
        Action::Refresh | Action::ToggleHidden | Action::Separator => {}
    }
}

/// 单项动作先核对菜单打开时的选集，再选中右键行；批量操作保持原选集。
fn select_menu_snapshot(
    pane: &mut FilesPane,
    snapshot: &FileMenuSnapshot,
    cx: &mut Context<FilesPane>,
) -> bool {
    if pane.show_hidden_files != snapshot.show_hidden {
        return false;
    }
    match &snapshot.target {
        FileMenuTarget::Background => {
            pane.activate_side(snapshot.side, cx);
            true
        }
        FileMenuTarget::List {
            entry,
            selected,
            ordered_names,
            ..
        } => {
            if pane.view_mode == FilesViewMode::Tree
                || !pane.panels[snapshot.side]
                    .listing
                    .as_ref()
                    .is_some_and(|listing| {
                        listing.entries.iter().any(|current| {
                            current.endpoint == entry.endpoint && current.kind == entry.kind
                        })
                    })
                || !select_context_selection(
                    pane,
                    snapshot.side,
                    selected,
                    snapshot.generation,
                    &snapshot.directory,
                    cx,
                )
            {
                return false;
            }
            select_context_entry(
                pane,
                snapshot.side,
                entry.clone(),
                ordered_names,
                snapshot.generation,
                &snapshot.directory,
                cx,
            )
        }
        FileMenuTarget::Tree {
            entry, selected, ..
        } => {
            if pane.view_mode != FilesViewMode::Tree
                || !pane.panels[snapshot.side]
                    .tree
                    .visible_endpoints(snapshot.show_hidden)
                    .contains(&entry.endpoint)
                || !pane.panels[snapshot.side]
                    .tree
                    .entry(&entry.endpoint)
                    .is_some_and(|current| current.kind == entry.kind)
                || !select_tree_selection(
                    pane,
                    snapshot.side,
                    selected,
                    snapshot.generation,
                    &snapshot.directory,
                    snapshot.show_hidden,
                    cx,
                )
            {
                return false;
            }
            select_tree_row(
                pane,
                snapshot.side,
                entry,
                snapshot.generation,
                &snapshot.directory,
                cx,
            )
        }
    }
}

/// 右键单行保留已选集合；未选行按捕获的可见顺序替换选择。
pub(super) fn select_context_entry(
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
    if !already_selected {
        pane.select_entry(side, entry, SelectionGesture::Replace, ordered_names, cx);
    }
    true
}

/// 列表批量命令仅接受与打开菜单时完全一致的条目，防止处理新选择。
pub(super) fn select_context_selection(
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
    if !same_selection(&current, expected) {
        report_selection_changed(pane, cx);
        return false;
    }
    pane.activate_side(side, cx);
    true
}

/// 树行使用完整 endpoint 区分同名节点，并保持已选树行的批量选择。
pub(super) fn select_tree_row(
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

/// 树批量命令以可见 endpoint 的顺序与类型校验，避免异步展开结果改变操作范围。
pub(super) fn select_tree_selection(
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
    if !same_selection(&current, expected) {
        report_selection_changed(pane, cx);
        return false;
    }
    pane.activate_side(side, cx);
    true
}

/// 比较完整路径和文件类型，避免同路径节点被替换后继续执行旧菜单。
fn same_selection(current: &[FileEntry], expected: &[FileEntry]) -> bool {
    current.len() == expected.len()
        && current
            .iter()
            .zip(expected)
            .all(|(left, right)| left.endpoint == right.endpoint && left.kind == right.kind)
}

/// 选区过期时给出可操作的恢复提示，不静默把动作转投给新选择。
fn report_selection_changed(pane: &mut FilesPane, cx: &mut Context<FilesPane>) {
    pane.status = Some(
        locale::text(
            "右键菜单中的选择已变化，请重新打开菜单。",
            "The context menu selection changed. Open the menu again.",
        )
        .to_string(),
    );
    cx.notify();
}

#[cfg(test)]
#[path = "../../../tests/unit/files_menu_policy.rs"]
mod tests;
