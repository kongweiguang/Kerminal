//! 文件面板内与操作系统文件管理器之间的拖放传输入口。
//!
//! @author kongweiguang

use std::sync::Arc;

use gpui_kit::{Context, IntoElement, ParentElement, Render, Styled, Window, div};
use kerminal_domain::files::{
    ConflictPolicy, FileEndpoint, FileEntry, FileEntryKind, TransferKind, TransferRequest,
};

use crate::{locale, theme::ThemeTokens};

mod external;

use super::{
    BatchEnqueueLabels, FilesPane, TransferBatchPlan, TransferEnqueueItem, child_endpoint,
    endpoint_name, parent_endpoint, valid_item_name,
};

/// 拖动载荷冻结来源面板、目录代次与选中根项，避免鼠标移动期间改用新目录。
#[derive(Clone)]
pub(super) struct FileTransferDrag {
    pub(super) source_side: usize,
    source_directory: FileEndpoint,
    source_generation: u64,
    entries: DraggedEntries,
    included_descendants: usize,
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
        source_side: usize,
        source_directory: FileEndpoint,
        source_generation: u64,
        entries: Arc<Vec<FileEntry>>,
        included_descendants: usize,
    ) -> Self {
        Self {
            source_side,
            source_directory,
            source_generation,
            entries: DraggedEntries::Selection(entries),
            included_descendants,
        }
    }

    /// 单行拖动直接把行快照放入载荷，避免为每个可见文件行分配临时 Vec。
    ///
    /// @author kongweiguang
    pub(super) fn single(
        source_side: usize,
        source_directory: FileEndpoint,
        source_generation: u64,
        entry: FileEntry,
    ) -> Self {
        Self {
            source_side,
            source_directory,
            source_generation,
            entries: DraggedEntries::Single(entry),
            included_descendants: 0,
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
}

impl FileDragPreview {
    /// 按主题构造不包含目标路径的拖动预览，避免拖拽时泄漏远端身份。
    ///
    /// @author kongweiguang
    pub(super) fn new(count: usize, theme: ThemeTokens) -> Self {
        Self {
            label: locale::text(format!("{count} 个项目"), format!("{count} items")).to_string(),
            theme,
        }
    }
}

impl Render for FileDragPreview {
    /// 将文件数量压缩为主题一致的小型反馈，避免预览携带路径或泄漏连接身份。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(self.theme.border)
            .bg(self.theme.secondary)
            .text_color(self.theme.foreground)
            .text_size(self.theme.text_sm)
            .child(self.label.clone())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DropRejection {
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
///
/// @author kongweiguang
pub(super) fn internal_drop_is_allowed(
    drag: &FileTransferDrag,
    current_source_side: usize,
    current_source: DropDirectorySnapshot<'_>,
    destination_side: usize,
    destination_directory: &FileEndpoint,
    destination_ready: bool,
    busy: bool,
) -> bool {
    if busy
        || !destination_ready
        || drag.source_side != current_source_side
        || !drop_directory_is_current(
            &drag.source_directory,
            drag.source_generation,
            current_source,
        )
        || drag.source_side == destination_side
        || drag.entries().is_empty()
    {
        return false;
    }

    drag.entries().iter().any(|entry| {
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
    let mut plan = InternalDropPlan {
        items: Vec::with_capacity(drag.entries().len()),
        issues: Vec::new(),
    };
    for entry in drag.entries() {
        let (name, kind) =
            match validate_drop_entry(entry, &drag.source_directory, destination_directory) {
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
    plan
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
    drag: &FileTransferDrag,
    destination_side: usize,
    destination_directory: &FileEndpoint,
    destination_generation: u64,
) -> bool {
    if drag.source_side >= pane.panels.len() || destination_side >= pane.panels.len() {
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

        let InternalDropPlan { items, issues } =
            plan_internal_drop(&drag, &destination_directory, self.queue.conflict_policy);
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
                total: drag.entries().len() + drag.included_descendants,
                included_by_selected_directories: drag.included_descendants,
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
fn drop_rejection_message(rejection: DropRejection) -> String {
    locale::text(
        match rejection {
            DropRejection::UnsupportedKind => "不传输符号链接或特殊文件。",
            DropRejection::InvalidName => "文件名无法安全表示。",
            DropRejection::SourceOutsideSnapshot => "项目已不属于拖动时的来源目录。",
            DropRejection::SameParent => "来源和目标是同一目录，已忽略无效复制。",
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
                "the item no longer belongs to the dragged source directory."
            }
            DropRejection::SameParent => {
                "source and target are the same directory; the no-op copy was ignored."
            }
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
