//! 文件面板内与操作系统文件管理器之间的拖放传输入口。
//!
//! @author kongweiguang

use std::{collections::HashSet, sync::Arc};

use gpui_kit::{
    Context, InteractiveElement as _, IntoElement, ParentElement, Pixels, Point, Render, Styled,
    Window, div,
};
use kerminal_domain::files::{
    ConflictPolicy, FileEndpoint, FileEntry, FileEntryKind, TransferKind, TransferRequest,
};

use crate::{locale, theme::ThemeTokens};

mod external;
#[cfg(windows)]
mod native_drag;

use super::{
    BatchEnqueueLabels, FilesPane, TransferBatchPlan, TransferEnqueueItem, child_endpoint,
    endpoint_name, parent_endpoint, valid_item_name,
};

/// 拖动载荷冻结来源面板、目录代次与选中根项，避免鼠标移动期间改用新目录。
#[derive(Clone)]
pub(super) struct FileTransferDrag {
    source_pane_id: u64,
    pub(super) source_side: usize,
    source_directory: FileEndpoint,
    source_generation: u64,
    entries: DraggedEntries,
    included_descendants: usize,
    tree_selection: Option<Arc<Vec<FileEntry>>>,
}

#[derive(Clone)]
enum DraggedEntries {
    Selection(Arc<Vec<FileEntry>>),
    Single(FileEntry),
}

impl FileTransferDrag {
    /// 复用同一份选择快照，列表多选时避免每行重新复制整组条目。
    ///
    /// @author kongweiguang
    pub(super) fn new(
        source_pane_id: u64,
        source_side: usize,
        source_directory: FileEndpoint,
        source_generation: u64,
        entries: Arc<Vec<FileEntry>>,
        included_descendants: usize,
    ) -> Self {
        Self {
            source_pane_id,
            source_side,
            source_directory,
            source_generation,
            entries: DraggedEntries::Selection(entries),
            included_descendants,
            tree_selection: None,
        }
    }

    /// 单行拖动直接把行快照放入载荷，避免为每个可见文件行分配临时 Vec。
    ///
    /// @author kongweiguang
    pub(super) fn single(
        source_pane_id: u64,
        source_side: usize,
        source_directory: FileEndpoint,
        source_generation: u64,
        entry: FileEntry,
    ) -> Self {
        Self {
            source_pane_id,
            source_side,
            source_directory,
            source_generation,
            entries: DraggedEntries::Single(entry),
            included_descendants: 0,
            tree_selection: None,
        }
    }

    /// 借用统一切片视图，使单项和多选路径共用同一组校验与队列逻辑。
    ///
    /// @author kongweiguang
    fn entries(&self) -> &[FileEntry] {
        match &self.entries {
            DraggedEntries::Selection(entries) => entries.as_slice(),
            DraggedEntries::Single(entry) => std::slice::from_ref(entry),
        }
    }

    /// 内部复制保留完整树选区，以便目标不安全的父目录不会吞掉可复制的已选子项。
    /// 外部拖出仍使用原有根项，避免向操作系统重复暴露父子路径。
    ///
    /// @author kongweiguang
    pub(super) fn with_tree_selection(mut self, entries: Arc<Vec<FileEntry>>) -> Self {
        self.tree_selection = Some(entries);
        self
    }

    /// 内部复制使用完整树选区；列表和单行仍使用冻结的普通拖动条目。
    ///
    /// @author kongweiguang
    fn copy_entries(&self) -> &[FileEntry] {
        self.tree_selection
            .as_deref()
            .map_or_else(|| self.entries(), |entries| entries.as_slice())
    }
    /// 预览优先展示原选择数量，树选择的已折叠子项也会计入用户提交的项目数。
    ///
    /// @author kongweiguang
    pub(super) fn selected_count(&self) -> usize {
        self.entries().len() + self.included_descendants
    }
}

/// 拖拽预览只显示项目数量并沿用当前主题令牌，不在跨窗口拖动时重建文件行。
pub(super) struct FileDragPreview {
    label: String,
    theme: ThemeTokens,
    cursor_offset: Point<Pixels>,
}

impl FileDragPreview {
    /// 保存 GPUI 相对整行的抓取偏移；紧凑预览需抵消它，才能始终贴近鼠标而非原行左缘。
    /// 预览只表达数量，英文按单复数呈现，避免单项拖动出现错误文案。
    ///
    /// @author kongweiguang
    pub(super) fn new(count: usize, cursor_offset: Point<Pixels>, theme: ThemeTokens) -> Self {
        Self {
            label: locale::text(
                format!("{count} 个项目"),
                format!("{count} {}", if count == 1 { "item" } else { "items" }),
            )
            .to_string(),
            theme,
            cursor_offset,
        }
    }
}

impl Render for FileDragPreview {
    /// GPUI 按鼠标减抓取偏移绘制根节点；偏移放在子节点，因为根布局会忽略自身的相对定位。
    /// 子节点抵消整行抓取偏移并留出间距，让紧凑预览贴近鼠标且不遮挡落点。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child(
            div()
                .relative()
                .left(self.cursor_offset.x)
                .top(self.cursor_offset.y)
                .debug_selector(|| "files-drag-preview".into())
                .ml_3()
                .mt_3()
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(self.theme.border)
                .bg(self.theme.secondary)
                .text_color(self.theme.foreground)
                .text_size(self.theme.text_sm)
                .child(self.label.clone()),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DropRejection {
    UnsupportedKind,
    InvalidName,
    SourceOutsideSnapshot,
    SameParent,
    SameObject,
    TargetInsideSourceDirectory,
    InvalidDestination,
}

/// 内部拖放计划保留逐项错误，使部分有效选择能继续入队并显式报告被拒项目。
struct InternalDropPlan {
    items: Vec<(String, TransferRequest)>,
    issues: Vec<String>,
    included_descendants: usize,
}

/// 列表快照同时绑定 endpoint 与 generation，阻止用户切换目录后旧拖动误投到新视图。
#[derive(Clone, Copy)]
pub(super) struct DropDirectorySnapshot<'a> {
    pub(super) endpoint: &'a FileEndpoint,
    pub(super) generation: u64,
    pub(super) loading: bool,
    pub(super) listing_directory: Option<&'a FileEndpoint>,
}

/// 判断系统文件管理器拖入的本机路径是否能进入当前远端或容器目录。
///
/// @author kongweiguang
pub(super) fn external_paths_drop_is_allowed(
    destination: &FileEndpoint,
    directory_ready: bool,
    busy: bool,
    has_paths: bool,
) -> bool {
    directory_ready && !busy && has_paths && !matches!(destination, FileEndpoint::Local { .. })
}

/// 判断单个源条目是否能复制到目标目录；相同目录、同一对象和目录自包含均不允许。
///
/// @author kongweiguang
fn validate_drop_entry(
    entry: &FileEntry,
    source_directory: &FileEndpoint,
    destination_directory: &FileEndpoint,
) -> Result<(String, TransferKind), DropRejection> {
    let kind = match entry.kind {
        FileEntryKind::File => TransferKind::File,
        FileEntryKind::Directory => TransferKind::Directory,
        FileEntryKind::Symlink | FileEntryKind::Other => {
            return Err(DropRejection::UnsupportedKind);
        }
    };
    let name = endpoint_name(&entry.endpoint).ok_or(DropRejection::InvalidName)?;
    if !valid_item_name(&name) || name != entry.name {
        return Err(DropRejection::InvalidName);
    }
    if !entry.endpoint.same_filesystem(source_directory)
        || !endpoint_is_same_or_descendant(&entry.endpoint, source_directory)
    {
        return Err(DropRejection::SourceOutsideSnapshot);
    }
    if parent_endpoint(&entry.endpoint).as_ref() == Some(destination_directory) {
        return Err(DropRejection::SameParent);
    }
    let destination =
        child_endpoint(destination_directory, &name).ok_or(DropRejection::InvalidDestination)?;
    if entry.endpoint.same_object(&destination) {
        return Err(DropRejection::SameObject);
    }
    if kind == TransferKind::Directory
        && endpoint_is_same_or_descendant(destination_directory, &entry.endpoint)
    {
        return Err(DropRejection::TargetInsideSourceDirectory);
    }
    Ok((name, kind))
}

/// 只在没有任何可复制项时返回原因；混合选区仍可提交安全项目并报告其余拒绝项。
///
/// @author kongweiguang
pub(super) fn copy_target_rejection<'a>(
    entries: impl IntoIterator<Item = &'a FileEntry>,
    source_directory: &FileEndpoint,
    destination_directory: &FileEndpoint,
) -> Option<DropRejection> {
    let mut rejection = None;
    for entry in entries {
        match validate_drop_entry(entry, source_directory, destination_directory) {
            Ok(_) => return None,
            Err(reason) if reason != DropRejection::UnsupportedKind && rejection.is_none() => {
                rejection = Some(reason);
            }
            Err(_) => {}
        }
    }
    rejection
}

/// 仅由安全的已选目录覆盖后代；被拒绝的父目录不能使其安全子项从批次消失。
///
/// @author kongweiguang
pub(super) fn copy_selection_roots(
    entries: Vec<FileEntry>,
    source_directory: &FileEndpoint,
    destination_directory: &FileEndpoint,
) -> (Vec<FileEntry>, usize) {
    let safe_directories = entries
        .iter()
        .filter(|entry| {
            entry.kind == FileEntryKind::Directory
                && validate_drop_entry(entry, source_directory, destination_directory).is_ok()
        })
        .map(|entry| entry.endpoint.clone())
        .collect::<HashSet<_>>();
    let total = entries.len();
    let roots = entries
        .into_iter()
        .filter(|entry| {
            let mut parent = parent_endpoint(&entry.endpoint);
            while let Some(endpoint) = parent {
                if safe_directories.contains(&endpoint) {
                    return false;
                }
                parent = parent_endpoint(&endpoint).filter(|next| next != &endpoint);
            }
            true
        })
        .collect::<Vec<_>>();
    let included_descendants = total - roots.len();
    (roots, included_descendants)
}

/// 父目录迭代只比较同一逻辑文件系统，覆盖本机 PathBuf 与 POSIX RemotePath 两种路径语义。
///
/// @author kongweiguang
fn endpoint_is_same_or_descendant(candidate: &FileEndpoint, ancestor: &FileEndpoint) -> bool {
    if !candidate.same_filesystem(ancestor) {
        return false;
    }
    if candidate == ancestor {
        return true;
    }

    let mut current = candidate.clone();
    while let Some(parent) = parent_endpoint(&current) {
        if &parent == ancestor {
            return true;
        }
        if parent == current {
            break;
        }
        current = parent;
    }
    false
}

/// 只有至少一个安全复制项、跨面板且两侧目录都已读取时，GPUI 才把内部拖动标成可放置。
/// 资格函数显式接收来源身份与两侧渲染快照，不为一次拖放增加另一份可变状态。
///
/// @author kongweiguang
#[allow(clippy::too_many_arguments)]
pub(super) fn internal_drop_is_allowed(
    drag: &FileTransferDrag,
    current_pane_id: u64,
    current_source_side: usize,
    current_source: DropDirectorySnapshot<'_>,
    destination_side: usize,
    destination_directory: &FileEndpoint,
    destination_ready: bool,
    busy: bool,
) -> bool {
    if busy
        || !destination_ready
        || drag.source_pane_id != current_pane_id
        || drag.source_side != current_source_side
        || !drop_directory_is_current(
            &drag.source_directory,
            drag.source_generation,
            current_source,
        )
        || drag.source_side == destination_side
        || drag.copy_entries().is_empty()
    {
        return false;
    }

    drag.copy_entries().iter().any(|entry| {
        validate_drop_entry(entry, &drag.source_directory, destination_directory).is_ok()
    })
}

/// 将拖动快照规划成统一传输请求；过滤项留在 issues 中供用户看见，而不是静默丢弃。
///
/// @author kongweiguang
fn plan_internal_drop(
    drag: &FileTransferDrag,
    destination_directory: &FileEndpoint,
    conflict_policy: ConflictPolicy,
) -> InternalDropPlan {
    let (selected, included_descendants) = if drag.tree_selection.is_some() {
        copy_selection_roots(
            drag.copy_entries().to_vec(),
            &drag.source_directory,
            destination_directory,
        )
    } else {
        (drag.entries().to_vec(), drag.included_descendants)
    };
    let (items, issues) = plan_copy_entries(
        &selected,
        &drag.source_directory,
        destination_directory,
        conflict_policy,
    );
    InternalDropPlan {
        items,
        issues,
        included_descendants,
    }
}

/// 菜单、工具栏和拖放共用逐项计划，避免同目录或目录自包含请求进入冲突预检。
///
/// @author kongweiguang
pub(super) fn plan_copy_entries(
    entries: &[FileEntry],
    source_directory: &FileEndpoint,
    destination_directory: &FileEndpoint,
    conflict_policy: ConflictPolicy,
) -> (Vec<(String, TransferRequest)>, Vec<String>) {
    let mut plan = InternalDropPlan {
        items: Vec::with_capacity(entries.len()),
        issues: Vec::new(),
        included_descendants: 0,
    };
    for entry in entries {
        let (name, kind) = match validate_drop_entry(entry, source_directory, destination_directory)
        {
            Ok(candidate) => candidate,
            Err(rejection) => {
                plan.issues.push(format!(
                    "{}: {}",
                    entry.name,
                    drop_rejection_message(rejection)
                ));
                continue;
            }
        };
        let Some(destination) = child_endpoint(destination_directory, &name) else {
            plan.issues.push(format!(
                "{}: {}",
                name,
                locale::text(
                    "目标文件系统不支持该名称。",
                    "The destination does not support this name."
                )
            ));
            continue;
        };
        plan.items.push((
            name,
            TransferRequest {
                source: entry.endpoint.clone(),
                destination,
                kind,
                conflict_policy,
                idle_timeout_seconds: None,
            },
        ));
    }
    (plan.items, plan.issues)
}

/// 只有拖动开始时记录的目录和代次仍显示对应列表时，拖放才可在释放时提交。
///
/// @author kongweiguang
fn drop_directory_is_current(
    expected_endpoint: &FileEndpoint,
    expected_generation: u64,
    current: DropDirectorySnapshot<'_>,
) -> bool {
    current.endpoint == expected_endpoint
        && current.generation == expected_generation
        && !current.loading
        && current.listing_directory == Some(expected_endpoint)
}

/// 面板在拖动释放时重新核对来源与目标代次，过期列表不能触发对旧路径的隐式操作。
/// 只在拖动开始时的来源与释放时的目标仍是原目录代次时接受内部复制。
///
/// @author kongweiguang
fn drop_targets_are_current(
    pane: &FilesPane,
    pane_id: u64,
    drag: &FileTransferDrag,
    destination_side: usize,
    destination_directory: &FileEndpoint,
    destination_generation: u64,
) -> bool {
    if drag.source_pane_id != pane_id
        || drag.source_side == destination_side
        || drag.source_side >= pane.panels.len()
        || destination_side >= pane.panels.len()
    {
        return false;
    }
    let source = &pane.panels[drag.source_side];
    let destination = &pane.panels[destination_side];
    !pane.transfer_submission_busy
        && drop_directory_is_current(
            &drag.source_directory,
            drag.source_generation,
            DropDirectorySnapshot {
                endpoint: &source.endpoint,
                generation: source.generation,
                loading: source.loading,
                listing_directory: source.listing.as_ref().map(|listing| &listing.directory),
            },
        )
        && drop_directory_is_current(
            destination_directory,
            destination_generation,
            DropDirectorySnapshot {
                endpoint: &destination.endpoint,
                generation: destination.generation,
                loading: destination.loading,
                listing_directory: destination
                    .listing
                    .as_ref()
                    .map(|listing| &listing.directory),
            },
        )
}

impl FilesPane {
    /// 冻结一次内部拖放的目标后交给统一队列；来源始终保留，操作语义为复制。
    ///
    /// @author kongweiguang
    pub(super) fn handle_internal_file_drop(
        &mut self,
        drag: FileTransferDrag,
        destination_side: usize,
        destination_directory: FileEndpoint,
        destination_generation: u64,
        cx: &mut Context<Self>,
    ) {
        if self.transfer_submission_busy {
            let _ = self.begin_transfer_submission(cx);
            return;
        }
        if !drop_targets_are_current(
            self,
            cx.entity_id().as_u64(),
            &drag,
            destination_side,
            &destination_directory,
            destination_generation,
        ) {
            self.status = Some(
                locale::text(
                    "目录已变化或尚未读取完成，请刷新后重新拖放。",
                    "A source or target directory changed. Refresh it and drop again.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let Some(target) = self.capture_transfer_target_snapshot(destination_side) else {
            self.status = Some(
                locale::text(
                    "目标目录尚未读取完成，请刷新后重新拖放。",
                    "The destination folder is not ready. Refresh it and drop again.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };

        let InternalDropPlan {
            items,
            issues,
            included_descendants,
        } = plan_internal_drop(&drag, &destination_directory, self.queue.conflict_policy);
        let items = items
            .into_iter()
            .map(|(name, request)| TransferEnqueueItem {
                name,
                external_target_lease: self.external_target_lease_for_request(&request),
                request,
            })
            .collect::<Vec<_>>();

        if items.is_empty() {
            self.status = Some(format!(
                "{} {}",
                locale::text("拖放未创建任务：", "No transfer was queued:"),
                issues.join("；")
            ));
            cx.notify();
            return;
        }

        self.enqueue_transfer_batch(
            TransferBatchPlan {
                items,
                total: drag.selected_count(),
                included_by_selected_directories: included_descendants,
                issues,
                labels: BatchEnqueueLabels {
                    action_zh: "拖放复制任务已入队",
                    action_en: "Dropped copy tasks queued",
                },
                target,
            },
            cx,
        );
    }
}

/// 将单项拖放拒绝原因投影为中英文可操作反馈，失败条目不会悄悄消失。
///
/// @author kongweiguang
pub(super) fn drop_rejection_message(rejection: DropRejection) -> String {
    locale::text(
        match rejection {
            DropRejection::UnsupportedKind => "不传输符号链接或特殊文件。",
            DropRejection::InvalidName => "文件名无法安全表示。",
            DropRejection::SourceOutsideSnapshot => "项目已不属于来源目录。",
            DropRejection::SameParent => "来源和目标是同一目录。",
            DropRejection::SameObject => "来源和目标指向同一项目。",
            DropRejection::TargetInsideSourceDirectory => "不能把目录复制到自身或其子目录。",
            DropRejection::InvalidDestination => "目标文件系统不支持该名称。",
        },
        match rejection {
            DropRejection::UnsupportedKind => {
                "symbolic links and special files are not transferred."
            }
            DropRejection::InvalidName => "the name cannot be represented safely.",
            DropRejection::SourceOutsideSnapshot => {
                "the item no longer belongs to the source directory."
            }
            DropRejection::SameParent => "source and target are the same directory.",
            DropRejection::SameObject => "source and target identify the same item.",
            DropRejection::TargetInsideSourceDirectory => {
                "a directory cannot be copied into itself or a descendant."
            }
            DropRejection::InvalidDestination => "the destination does not support this name.",
        },
    )
    .to_string()
}

/// 同一目标是否已有可见读取结果，过期列表不接受操作系统拖入的文件。
///
/// @author kongweiguang
pub(super) fn external_drop_directory_ready(
    endpoint: &FileEndpoint,
    listing_directory: Option<&FileEndpoint>,
    loading: bool,
) -> bool {
    !loading && listing_directory == Some(endpoint)
}

#[cfg(test)]
#[path = "../../../tests/unit/files_drag_drop.rs"]
mod drag_drop_tests;
