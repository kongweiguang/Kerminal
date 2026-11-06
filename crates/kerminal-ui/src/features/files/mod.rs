//! 本机与远端文件工作区；页面只编排文件、书签与传输用例。
//!
//! @author kongweiguang

mod context_menu;
mod dialog;
mod directory_follow;
mod drag_drop;
mod host_picker;
mod host_tabs;
mod permissions;
mod remote_clipboard;
mod render;
#[cfg(feature = "right-rail-acceptance")]
mod right_rail_acceptance;
mod selection;
mod target_names;
mod transfer_queue;
mod transfer_refresh;
#[cfg(feature = "right-rail-acceptance")]
mod transfer_workspace_acceptance;
mod tree;
mod view_mode;

pub use host_picker::{
    CreateSftpHostRequested, FilePaneSide, SftpHostKeyTrustRequested, SftpHostRequested,
};
#[cfg(feature = "right-rail-acceptance")]
pub use right_rail_acceptance::transfers::{
    RightRailSftpTransferItem, RightRailSftpTransferTarget,
};
pub(crate) use target_names::{FileTargetNameLookup, FileTargetNameState, SftpHostChoice};

use std::{
    collections::HashSet,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use gpui_kit::component::tree::{TreeEvent, TreeState};
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, KeyDownEvent, PathPromptOptions, Subscription,
    Window,
    base::VirtualListScrollHandle,
    component::input::{InputEvent, InputState},
};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor, PublicErrorCode,
    files::FileService,
    platform::external_launch::ExternalTargetLease,
    preferences::{InterfaceDensity, ThemeMode},
    sessions::UiSessionBinding,
    transfers::{TransferOrigin, TransferQueueEvent, TransferRecoveryRecord, TransferService},
};
use kerminal_domain::files::{
    BookmarkTarget, ConflictPolicy, DeleteTargetKind, DirectoryListing, FileBookmark, FileEndpoint,
    FileEntry, FileEntryKind, RemotePath, TransferKind, TransferRequest, TransferSummary,
};
use kerminal_domain::identity::HostId;
use tokio_util::sync::CancellationToken;

use crate::locale;
use transfer_refresh::{
    TransferRefreshScope, completed_transfer_refresh_directories, panel_needs_transfer_refresh,
};

/// 文件面板冻结编辑目标及其独立临时连接 owner，避免切换终端后文档失去原目标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenFileRequested {
    pub endpoint: FileEndpoint,
    pub external_target_lease: Option<ExternalTargetLease>,
}

/// 工作区中的文件行请求 Root 激活仍然存在的 editor endpoint。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActivateEditorFileRequested {
    pub(crate) endpoint: FileEndpoint,
}

/// 文件面板只向 Root 投递端点变化与主机选择，不在 UI 安装远端 Shell 配置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FilesPaneEvent {
    TransferEndpointsChanged {
        left: FileEndpoint,
        right: Option<FileEndpoint>,
    },
    CancelHostSelection {
        side: FilePaneSide,
        generation: u64,
    },
}

/// 恢复与权限回读要求目标仍存在，传输目标则可能尚未发布到父目录。
///
/// @author kongweiguang
enum PendingSelection {
    Required(String),
    IfPresent(String),
}

/// 左右面板各自维护当前目录和选择，切换目标不会重建后台传输任务。
struct BrowserPanel {
    endpoint: FileEndpoint,
    /// 根目录读取的取消 owner 独立于 FileTreeBrowser 的子树请求生命周期。
    root_request_cancellation: Option<RootDirectoryRequestCancellation>,
    /// 正在读取时保留用户最新请求的目录；成功回执前 endpoint 仍是旧的已提交清单位置。
    requested_endpoint: Option<FileEndpoint>,
    listing: Option<DirectoryListing>,
    selected: Option<FileEntry>,
    selection: selection::FileSelection,
    /// 每侧只持有一个列表焦点句柄，虚拟行滚动复用同一复合控件焦点。
    list_focus_handle: Option<gpui_kit::FocusHandle>,
    keyboard_focus: FileListKeyboardFocus,
    pending_selection: Option<PendingSelection>,
    path_input: Option<Entity<InputState>>,
    path_input_endpoint: Option<String>,
    _path_subscription: Option<Subscription>,
    loading: bool,
    directory_error: Option<String>,
    /// 失败读取仍指向原请求，重试不能退回保留旧清单的已提交目录。
    failed_directory: Option<FileEndpoint>,
    generation: u64,
    tree: tree::FileTreeBrowser,
    tree_widget: Option<tree::TreeWidget>,
    list_scroll: VirtualListScrollHandle,
    sort: FileSort,
    entry_filter: view_mode::FileEntryFilter,
    navigation_history: NavigationHistory,
}

struct RootDirectoryRequestCancellation {
    generation: u64,
    cancellation: CancellationToken,
}

/// 键盘活动项按目录与文件名绑定；虚拟列表滚动和排序不会留下行索引焦点。
///
/// @author kongweiguang
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct FileListKeyboardFocus {
    directory: Option<FileEndpoint>,
    active_name: Option<String>,
}

/// 键盘事件绑定到绘制帧的目标、顺序和尺寸，避免在缩放或切换目录时混用不同快照。
///
/// @author kongweiguang
struct FileListNavigation<'a> {
    side: usize,
    generation: u64,
    directory: &'a FileEndpoint,
    ordered_names: &'a [String],
    row_height: f32,
}

/// 将文件列表行高映射到 main 与全局界面偏好共用的三个密度等级。
///
/// @author kongweiguang
pub(super) fn file_list_row_height(density: InterfaceDensity) -> f32 {
    match density {
        InterfaceDensity::Compact => 36.0,
        InterfaceDensity::Comfortable => 44.0,
        InterfaceDensity::Spacious => 48.0,
    }
}

/// 支持相邻、按视口翻页与首尾定位；翻页留一行上下文并在列表边界钳制。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FileListMovement {
    Previous,
    Next,
    PagePrevious { visible_rows: usize },
    PageNext { visible_rows: usize },
    First,
    Last,
}

impl FileListKeyboardFocus {
    /// 目录身份改变时清除活动文件；同目录刷新会由可见项回读再裁剪。
    ///
    /// @author kongweiguang
    fn reset_for_directory(&mut self, directory: &FileEndpoint) {
        if self.directory.as_ref() != Some(directory) {
            self.directory = Some(directory.clone());
            self.active_name = None;
        }
    }

    /// 目录回读后清理已消失项，避免旧文件名成为下一次范围移动的起点。
    ///
    /// @author kongweiguang
    fn retain_visible(&mut self, directory: &FileEndpoint, visible_names: &[String]) {
        self.reset_for_directory(directory);
        if self
            .active_name
            .as_ref()
            .is_some_and(|name| !visible_names.iter().any(|visible| visible == name))
        {
            self.active_name = None;
        }
    }

    /// 将鼠标或键盘命中绑定到当前目录中的稳定名称，而不是虚拟行的瞬时索引。
    ///
    /// @author kongweiguang
    fn set_active(&mut self, directory: &FileEndpoint, name: &str, visible_names: &[String]) {
        self.retain_visible(directory, visible_names);
        if visible_names.iter().any(|visible| visible == name) {
            self.active_name = Some(name.to_owned());
        }
    }

    /// 按当前投影移动活动项；翻页以视口行数减一保留上下文，空视口也至少前进一行。
    ///
    /// @author kongweiguang
    pub(super) fn move_active(
        &mut self,
        directory: &FileEndpoint,
        visible_names: &[String],
        movement: FileListMovement,
    ) -> Option<usize> {
        self.retain_visible(directory, visible_names);
        let last = visible_names.len().checked_sub(1)?;
        let current = self
            .active_name
            .as_ref()
            .and_then(|active| visible_names.iter().position(|name| name == active));
        let next = match movement {
            FileListMovement::Previous => current.map_or(last, |index| index.saturating_sub(1)),
            FileListMovement::Next => current.map_or(0, |index| index.saturating_add(1).min(last)),
            FileListMovement::PagePrevious { visible_rows } => current.map_or(last, |index| {
                index.saturating_sub(visible_rows.saturating_sub(1).max(1))
            }),
            FileListMovement::PageNext { visible_rows } => current.map_or(0, |index| {
                index
                    .saturating_add(visible_rows.saturating_sub(1).max(1))
                    .min(last)
            }),
            FileListMovement::First => 0,
            FileListMovement::Last => last,
        };
        self.active_name = Some(visible_names[next].clone());
        Some(next)
    }

    /// 仅对当前目录且仍处于当前投影中的项目报告活动身份，供渲染和操作入口复核。
    ///
    /// @author kongweiguang
    pub(super) fn active_name<'a>(
        &'a self,
        directory: &FileEndpoint,
        visible_names: &[String],
    ) -> Option<&'a str> {
        if self.directory.as_ref() != Some(directory) {
            return None;
        }
        self.active_name
            .as_deref()
            .filter(|active| visible_names.iter().any(|name| name == active))
    }

    /// 切换到无可见目标的文件系统时撤销旧路径上的活动行。
    ///
    /// @author kongweiguang
    fn clear(&mut self) {
        self.directory = None;
        self.active_name = None;
    }
}

/// 每个浏览侧只记录当前路径是否离开过初始目录；共享工作台据此决定是否应用主机本地起始目录。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
struct NavigationHistory {
    current: FileEndpoint,
    moved: bool,
}

impl NavigationHistory {
    /// 初始化时当前目录就是初始目录。
    ///
    /// @author kongweiguang
    fn new(endpoint: FileEndpoint) -> Self {
        Self {
            current: endpoint,
            moved: false,
        }
    }

    /// 切换连接身份时重置为新的初始目录，避免旧目标影响共享工作台判断。
    ///
    /// @author kongweiguang
    fn reset(&mut self, endpoint: FileEndpoint) {
        self.current = endpoint;
        self.moved = false;
    }

    /// 只有真正切换目录才标记离开初始路径；重复打开当前目录不改变判断。
    ///
    /// @author kongweiguang
    fn push(&mut self, endpoint: FileEndpoint) {
        if self.current == endpoint {
            return;
        }
        self.current = endpoint;
        self.moved = true;
    }

    /// 非初始目录只用于首次绑定主机时的默认目录决策。
    ///
    /// @author kongweiguang
    fn is_initial(&self) -> bool {
        !self.moved
    }
}

/// 紧凑文件栏区分失效、切换文件系统、保留本机浏览位置和显式 SSH 目录跟随。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompactTargetUpdateAction {
    Unavailable,
    BindTarget,
    PreservePath,
    FollowDirectory,
    Unchanged,
}

/// 本机终端共享同一文件系统，切换本机 Tab 或 CWD 不覆盖手动目录；SSH 仍按主机校验并尊重显式跟随。
///
/// @author kongweiguang
fn compact_target_update_action(
    previous_terminal: Option<&FileEndpoint>,
    browser_endpoint: &FileEndpoint,
    next_terminal: Option<&FileEndpoint>,
    follow_directory: bool,
) -> CompactTargetUpdateAction {
    let Some(next_terminal @ (FileEndpoint::Local { .. } | FileEndpoint::Remote { .. })) =
        next_terminal
    else {
        return CompactTargetUpdateAction::Unavailable;
    };
    if previous_terminal == Some(next_terminal) {
        return CompactTargetUpdateAction::Unchanged;
    }
    let same_filesystem = previous_terminal
        .is_some_and(|previous| endpoint_same_filesystem(previous, next_terminal))
        && endpoint_same_filesystem(browser_endpoint, next_terminal);
    if !same_filesystem {
        CompactTargetUpdateAction::BindTarget
    } else if !follow_directory || matches!(next_terminal, FileEndpoint::Local { .. }) {
        CompactTargetUpdateAction::PreservePath
    } else if browser_endpoint == next_terminal {
        CompactTargetUpdateAction::Unchanged
    } else {
        CompactTargetUpdateAction::FollowDirectory
    }
}

/// 共享工作台先展示本机目录，只有首次选中远端且左侧仍停留在初始路径时才采用主机本地起始目录。
///
/// @author kongweiguang
fn should_apply_shared_host_local_start(
    host_picker_enabled: bool,
    right_target_unselected: bool,
    left_endpoint: &FileEndpoint,
    left_path_is_initial: bool,
    left_host_locked: bool,
) -> bool {
    host_picker_enabled
        && right_target_unselected
        && matches!(left_endpoint, FileEndpoint::Local { .. })
        && left_path_is_initial
        && !left_host_locked
}

/// 中央工作台两端都必须明确选择，避免内部本机占位被当作传输来源或目标。
///
/// @author kongweiguang
fn can_transfer_to_other_side(
    host_picker_enabled: bool,
    target_unselected: [bool; 2],
    source_side: usize,
) -> bool {
    source_side < 2 && (!host_picker_enabled || !target_unselected.into_iter().any(|value| value))
}

/// 文件列表排序仅属于各自浏览面板，避免视图排序影响冻结的传输目标。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct FileSort {
    pub(super) column: FileSortColumn,
    pub(super) descending: bool,
}

/// 只对真实存在的名称、大小和修改时间列提供排序。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum FileSortColumn {
    #[default]
    Name,
    Size,
    Modified,
}

impl FileSort {
    /// 同一列重复点击反转顺序，改选列时从升序开始，保持目录优先规则不变。
    ///
    /// @author kongweiguang
    fn toggle(&mut self, column: FileSortColumn) {
        if self.column == column {
            self.descending = !self.descending;
        } else {
            self.column = column;
            self.descending = false;
        }
    }
}

/// GPUI 文件工作区维持左右浏览、书签与统一传输队列的轻量视图状态。
pub struct FilesPane {
    files: FileService,
    transfers: TransferService,
    executor: ApplicationTaskExecutor,
    target_name_lookup: FileTargetNameLookup,
    target_names: FileTargetNameState,
    panels: [BrowserPanel; 2],
    directory_navigation_generations: [u64; 2],
    local_directories: [PathBuf; 2],
    locked_left_host_id: Option<HostId>,
    external_target_lease: Option<ExternalTargetLease>,
    active_side: usize,
    follow_terminal_directory: bool,
    current_terminal_endpoint: Option<FileEndpoint>,
    current_terminal_binding: Option<UiSessionBinding>,
    /// 只标记自动跟随发起的根读取；关闭开关不能取消用户手动导航。
    terminal_directory_follow_generation: Option<u64>,
    bookmarks: Vec<FileBookmark>,
    bookmarks_target: Option<BookmarkTarget>,
    bookmark_request_target: Option<BookmarkTarget>,
    bookmark_generation: u64,
    bookmark_error: Option<String>,
    bookmark_error_target: Option<BookmarkTarget>,
    transfer_origin: Option<TransferOrigin>,
    all_transfer_rows: Vec<TransferSummary>,
    transfer_rows: Vec<TransferSummary>,
    transfer_recovery_rows: Vec<TransferRecoveryRecord>,
    queue: transfer_queue::TransferQueueUi,
    host_picker: host_picker::HostPickerState,
    transfer_submission_busy: bool,
    transfer_submission_generation: u64,
    transfer_submission: Option<TransferSubmission>,
    #[cfg(target_os = "windows")]
    drag_out_generation: u64,
    pending_transfer_conflict: Option<PendingTransferConflict>,
    permissions: permissions::PermissionEditor,
    compact_layout: bool,
    view_mode: view_mode::FilesViewMode,
    show_hidden_files: bool,
    status: Option<String>,
    status_details_expanded: bool,
    file_row_height: f32,
    initialized: bool,
    queue_watch_started: bool,
    delete_confirmation: Option<Vec<FileEntry>>,
    delete_action_busy: bool,
    next_host_key_trust_request_id: u64,
    pending_host_key_trust: Option<PendingHostKeyTrust>,
    name_input: Option<Entity<InputState>>,
    _name_subscription: Option<Subscription>,
    name_action: Option<NameAction>,
    name_action_side: Option<usize>,
    name_action_target: Option<FileEndpoint>,
    name_action_busy: bool,
    permission_dialog_target: Option<(usize, FileEndpoint)>,
    global_dialog_close_focus: Option<gpui_kit::FocusHandle>,
}

/// 共用短名称输入仅表达当前待执行的单项文件动作，不缓存或拼接不可信路径。
#[derive(Clone, Copy, PartialEq, Eq)]
enum NameAction {
    CreateDirectory,
    Rename,
}

/// 固定 trust 发起时的目录和读取代次，回执只可刷新仍然显示该目录的文件栏。
#[derive(Clone)]
struct PendingHostKeyTrust {
    request: SftpHostKeyTrustRequested,
    endpoint: FileEndpoint,
    panel_generation: u64,
}

/// 上传选择器按菜单命令限制系统选择范围。
#[derive(Clone, Copy)]
enum UploadSelectionKind {
    Files,
    Directories,
}

/// 一个批次的文件目标始终携带原请求租约，避免异步入队时改用当前活动主机。
struct TransferEnqueueItem {
    name: String,
    request: TransferRequest,
    external_target_lease: Option<ExternalTargetLease>,
}

/// 预检和弹框确认固定同一个浏览面板快照，目标变化后不得把未提交请求投向新目录。
#[derive(Clone)]
struct TransferTargetSnapshot {
    side: usize,
    endpoint: FileEndpoint,
    generation: u64,
}

/// 一个已构造但尚未入队的批次；冲突策略只在此快照内变更，不修改队列默认值。
struct TransferBatchPlan {
    items: Vec<TransferEnqueueItem>,
    total: usize,
    included_by_selected_directories: usize,
    issues: Vec<String>,
    labels: BatchEnqueueLabels,
    target: TransferTargetSnapshot,
}

/// 提交前可撤销，入队后必须等待逐项回执，避免视图生命周期丢失真实结果。
#[derive(Clone, Copy, PartialEq, Eq)]
enum TransferSubmissionPhase {
    Preparing,
    Enqueuing,
}

/// 同一面板至多一个提交，代次隔离迟到回执，侧别限定目录变化的取消范围。
struct TransferSubmission {
    generation: u64,
    origin: TransferOrigin,
    cancellation: CancellationToken,
    related_sides: [bool; 2],
    phase: TransferSubmissionPhase,
}

/// 只有已确认存在冲突的批次才跨帧保留，用于主窗口级决策弹框。
struct PendingTransferConflict {
    plan: TransferBatchPlan,
    conflicts: Vec<TransferConflict>,
}

/// 区分已有目标与批次内重复目标，避免将尚未创建的文件描述为已经存在。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransferConflictKind {
    Existing,
    Repeated,
}

/// 索引只引用同一冻结计划，保留原请求及租约的唯一所有权。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TransferConflict {
    item_index: usize,
    kind: TransferConflictKind,
}

impl PendingTransferConflict {
    /// 预检索引与冻结计划一起生成，投影时沿用原请求，不重新读取可能变化的目录。
    ///
    /// @author kongweiguang
    fn conflict_items(&self) -> impl Iterator<Item = (&TransferRequest, TransferConflictKind)> {
        self.conflicts
            .iter()
            .map(|conflict| (&self.plan.items[conflict.item_index].request, conflict.kind))
    }
}

/// 下载对话框完成前保存已验证的来源，目标路径只在用户确认本机目录后构造。
struct DownloadSourceItem {
    name: String,
    source: FileEndpoint,
    kind: TransferKind,
    external_target_lease: Option<ExternalTargetLease>,
}

/// 队列逐项返回受理结果；已受理项目不会因后续取消或失败被误报为未处理。
#[derive(Default)]
struct BatchEnqueueOutcome {
    total: usize,
    planned_tasks: usize,
    queued: usize,
    included_by_selected_directories: usize,
    issues: Vec<String>,
    cancelled_remaining: usize,
}

/// 入队反馈的中英文标签成对保存，避免异步回执错配用户操作文案。
///
/// @author kongweiguang
struct BatchEnqueueLabels {
    action_zh: &'static str,
    action_en: &'static str,
}

/// 删除批处理逐项确认结果；失败项留在列表中供用户重新检查和选择。
#[derive(Default)]
struct BatchDeleteOutcome {
    total: usize,
    deleted: usize,
    issues: Vec<String>,
    cancelled_remaining: usize,
}

/// 执行已经二次确认的删除批次，逐项取消并保留失败目标的错误原因。
///
/// @author kongweiguang
async fn delete_file_entries(
    files: FileService,
    cancellation: CancellationToken,
    items: Vec<(
        String,
        FileEndpoint,
        DeleteTargetKind,
        Option<ExternalTargetLease>,
    )>,
) -> BatchDeleteOutcome {
    let total = items.len();
    let mut outcome = BatchDeleteOutcome {
        total,
        ..BatchDeleteOutcome::default()
    };
    for (index, (name, endpoint, kind, lease)) in items.into_iter().enumerate() {
        if cancellation.is_cancelled() {
            outcome.cancelled_remaining = total.saturating_sub(index);
            break;
        }
        let _external_target_scope = lease;
        match files
            .delete(cancellation.child_token(), endpoint, kind)
            .await
        {
            Ok(()) => outcome.deleted += 1,
            Err(error) => outcome
                .issues
                .push(format!("{name}: {}", localized_error(&error))),
        }
    }
    outcome
}

/// 顺序提交批次中的入队请求，取消只停止后继项且 Unknown 由队列保持原终态。
///
/// @author kongweiguang
async fn enqueue_transfer_items(
    transfers: TransferService,
    origin: TransferOrigin,
    cancellation: CancellationToken,
    items: Vec<TransferEnqueueItem>,
    total: usize,
    included_by_selected_directories: usize,
    issues: Vec<String>,
) -> BatchEnqueueOutcome {
    let items_count = items.len();
    let mut outcome = BatchEnqueueOutcome {
        total,
        planned_tasks: items_count,
        queued: 0,
        included_by_selected_directories,
        issues,
        cancelled_remaining: 0,
    };
    for (index, item) in items.into_iter().enumerate() {
        if cancellation.is_cancelled() {
            outcome.cancelled_remaining = items_count.saturating_sub(index);
            break;
        }
        let result = transfers
            .enqueue_for_origin(
                item.request,
                origin.clone(),
                item.external_target_lease.as_ref(),
            )
            .await;
        match result {
            Ok(_) => outcome.queued += 1,
            Err(error) => {
                outcome
                    .issues
                    .push(format!("{}: {}", item.name, localized_error(&error)))
            }
        }
    }
    outcome
}

/// 按父目录核验目标并保留原批次索引；任一目录读取失败时整批阻断，不把未知目标误报为无冲突。
///
/// @author kongweiguang
async fn find_transfer_conflicts(
    files: &FileService,
    cancellation: &CancellationToken,
    items: &[TransferEnqueueItem],
) -> AppResult<Vec<TransferConflict>> {
    let mut directories: Vec<(FileEndpoint, Vec<(usize, FileEndpoint)>)> = Vec::new();
    for (item_index, item) in items.iter().enumerate() {
        let destination = item.request.destination.clone();
        let parent = parent_endpoint(&destination)
            .ok_or_else(|| AppError::invalid("传输目标没有可检查的父目录"))?;
        if let Some((_, targets)) = directories.iter_mut().find(|(path, _)| *path == parent) {
            targets.push((item_index, destination));
        } else {
            directories.push((parent, vec![(item_index, destination)]));
        }
    }

    let mut conflicts = Vec::new();
    for (parent, targets) in directories {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("传输冲突预检已取消"));
        }
        let listing = files
            .list_directory(cancellation.child_token(), parent.clone())
            .await?;
        if listing.directory != parent {
            return Err(AppError::conflict("冲突预检返回的目录与目标不一致"));
        }
        let existing = listing
            .entries
            .into_iter()
            .map(|entry| entry.endpoint)
            .collect::<HashSet<_>>();
        conflicts.extend(existing_transfer_conflicts(&targets, &existing));
    }
    conflicts.sort_by_key(|conflict| conflict.item_index);
    Ok(conflicts)
}

/// 已有目标逐项标记；尚不存在的重复目标只标记后续请求，并保留真实冲突原因供界面说明。
///
/// @author kongweiguang
fn existing_transfer_conflicts(
    targets: &[(usize, FileEndpoint)],
    existing: &HashSet<FileEndpoint>,
) -> Vec<TransferConflict> {
    let mut planned = HashSet::with_capacity(targets.len());
    targets
        .iter()
        .filter_map(|(item_index, target)| {
            let kind = if existing.contains(target) {
                TransferConflictKind::Existing
            } else if !planned.insert(target.clone()) {
                TransferConflictKind::Repeated
            } else {
                return None;
            };
            Some(TransferConflict {
                item_index: *item_index,
                kind,
            })
        })
        .collect()
}

/// 全部受理的结果已在传输列表可见，只对未受理、部分失败或取消保留反馈，避免成功底栏挤动列表。
///
/// @author kongweiguang
fn batch_enqueue_message(
    action_zh: &str,
    action_en: &str,
    outcome: BatchEnqueueOutcome,
) -> Option<String> {
    if outcome.issues.is_empty()
        && outcome.cancelled_remaining == 0
        && outcome.queued == outcome.planned_tasks
    {
        return None;
    }
    let mut message = if outcome.included_by_selected_directories == 0 {
        format!(
            "{} {}/{}.",
            locale::text(action_zh, action_en),
            outcome.queued,
            outcome.total
        )
    } else {
        format!(
            "{} {}/{} {}.",
            locale::text(action_zh, action_en),
            outcome.queued,
            outcome.planned_tasks,
            locale::text("个传输任务已受理", "transfer tasks accepted")
        )
    };
    if outcome.cancelled_remaining > 0 {
        message.push_str(&format!(
            " {} {} {}.",
            locale::text("已取消后续处理，剩余", "Cancelled remaining"),
            outcome.cancelled_remaining,
            locale::text("项未入队", "items were not queued")
        ));
    }
    if outcome.included_by_selected_directories > 0 {
        message.push_str(&format!(
            " {}",
            locale::text(
                format!(
                    "{} 个已选后代不单独入队，其处理取决于所选目录任务是否受理。",
                    outcome.included_by_selected_directories
                ),
                format!(
                    "{} selected descendants are not queued separately; their handling depends on whether the selected directory task is accepted.",
                    outcome.included_by_selected_directories
                )
            )
        ));
    }
    if !outcome.issues.is_empty() {
        message.push_str(&format!(
            " {}: {}",
            locale::text("未入队", "Not queued"),
            outcome.issues.join(&locale::text("；", "; "))
        ));
    }
    Some(message)
}

impl FilesPane {
    /// 组合根传入的 Local endpoint 是浏览起点，左右本机栏都从该目录开始而不依赖进程启动目录。
    /// 写操作的导航身份独立于读取代次，普通刷新不能使在途结果失去反馈。
    ///
    /// @author kongweiguang
    pub(crate) fn new(
        files: FileService,
        transfers: TransferService,
        executor: ApplicationTaskExecutor,
        target_name_lookup: FileTargetNameLookup,
        endpoint: FileEndpoint,
    ) -> Self {
        let local_directory = match &endpoint {
            FileEndpoint::Local { path } => path.clone(),
            _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        let local = FileEndpoint::Local {
            path: local_directory.clone(),
        };
        let target = endpoint;
        Self {
            files,
            transfers,
            executor,
            target_name_lookup,
            target_names: FileTargetNameState::default(),
            panels: [BrowserPanel::new(local), BrowserPanel::new(target)],
            directory_navigation_generations: [0, 0],
            local_directories: [local_directory.clone(), local_directory],
            locked_left_host_id: None,
            external_target_lease: None,
            active_side: 1,
            follow_terminal_directory: false,
            current_terminal_endpoint: None,
            current_terminal_binding: None,
            terminal_directory_follow_generation: None,
            bookmarks: Vec::new(),
            bookmarks_target: None,
            bookmark_request_target: None,
            bookmark_generation: 0,
            bookmark_error: None,
            bookmark_error_target: None,
            transfer_origin: Some(TransferOrigin::SharedWorkbench),
            all_transfer_rows: Vec::new(),
            transfer_rows: Vec::new(),
            transfer_recovery_rows: Vec::new(),
            queue: transfer_queue::TransferQueueUi::default(),
            host_picker: host_picker::HostPickerState::default(),
            transfer_submission_busy: false,
            transfer_submission_generation: 0,
            transfer_submission: None,
            #[cfg(target_os = "windows")]
            drag_out_generation: 0,
            pending_transfer_conflict: None,
            permissions: permissions::PermissionEditor::default(),
            compact_layout: false,
            view_mode: view_mode::FilesViewMode::List,
            show_hidden_files: true,
            status: None,
            status_details_expanded: false,
            file_row_height: file_list_row_height(InterfaceDensity::Compact),
            initialized: false,
            queue_watch_started: false,
            delete_confirmation: None,
            delete_action_busy: false,
            next_host_key_trust_request_id: 0,
            pending_host_key_trust: None,
            name_input: None,
            _name_subscription: None,
            name_action: None,
            name_action_side: None,
            name_action_target: None,
            name_action_busy: false,
            permission_dialog_target: None,
            global_dialog_close_focus: None,
        }
    }

    /// 按宿主实际槽位切换紧凑展示，避免用整窗宽度误判右侧 dock 的可用空间。
    ///
    /// @author kongweiguang
    pub fn set_compact_layout(&mut self, compact: bool, cx: &mut Context<Self>) {
        if self.compact_layout == compact {
            return;
        }
        self.compact_layout = compact;
        if compact {
            self.set_transfer_origin(None, cx);
        }
        if !compact {
            self.permission_dialog_target = None;
        }
        if !compact && self.initialized && self.panels[0].listing.is_none() {
            self.load_panel(0, cx);
        }
        cx.notify();
    }

    /// 切换来源立即撤销旧投影和未受理操作，已入队批次继续使用提交时冻结的身份。
    ///
    /// @author kongweiguang
    pub(crate) fn set_transfer_origin(
        &mut self,
        origin: Option<TransferOrigin>,
        cx: &mut Context<Self>,
    ) {
        if self.transfer_origin == origin {
            return;
        }
        self.cancel_transfer_preparation(cx);
        self.transfer_origin = origin;
        self.transfer_rows.clear();
        self.transfer_recovery_rows.clear();
        let generation = self.queue.begin_transfer_refresh();
        self.queue.finish_recovery_refresh(generation, Ok(()));
        if self.initialized {
            self.reload_transfers(cx);
        }
        cx.notify();
    }

    /// 关闭门禁只读取当前面板来源，避免把另一 pane/tab 的入队状态当成目标来源状态。
    ///
    /// @author kongweiguang
    pub(crate) fn transfer_origin_matches(&self, origin: &TransferOrigin) -> bool {
        self.transfer_origin.as_ref() == Some(origin)
    }

    /// 复用全局界面密度偏好驱动文件行高，确保分页键步幅与真实可见行数一致。
    ///
    /// @author kongweiguang
    pub(crate) fn set_interface_density(
        &mut self,
        density: InterfaceDensity,
        cx: &mut Context<Self>,
    ) {
        let row_height = file_list_row_height(density);
        if self.file_row_height == row_height {
            return;
        }
        self.file_row_height = row_height;
        cx.notify();
    }

    /// 表头切换排序列只重绘列表，不重新读取目录或触碰传输任务。
    ///
    /// @author kongweiguang
    pub(super) fn toggle_sort(
        &mut self,
        side: usize,
        column: FileSortColumn,
        cx: &mut Context<Self>,
    ) {
        self.panels[side].sort.toggle(column);
        cx.notify();
    }

    /// 文件类型筛选只改变当前侧可见列表，不重新读取目录或移除原选择。
    ///
    /// @author kongweiguang
    fn set_file_entry_filter(
        &mut self,
        side: usize,
        filter: view_mode::FileEntryFilter,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.panels.get_mut(side) else {
            return;
        };
        if panel.entry_filter == filter {
            return;
        }
        panel.entry_filter = filter;
        cx.notify();
    }

    /// 会话身份与可信目录原子同步，避免同主机换终端被误判成文件系统切换而丢失清单和取消资格。
    /// 文件来源没有终端 binding，切换文件时采用其父目录；隐藏的跟随开关不影响文件来源。
    /// 目标失效仍撤销旧数据；跟随以在途目录为准，避免 A→B→A 的慢响应覆盖最新位置。
    ///
    /// @author kongweiguang
    ///
    pub(crate) fn set_current_terminal_target(
        &mut self,
        endpoint: Option<FileEndpoint>,
        lease: Option<ExternalTargetLease>,
        binding: Option<UiSessionBinding>,
        cx: &mut Context<Self>,
    ) {
        let binding_changed = self.current_terminal_binding != binding;
        let file_source = binding.is_none();
        let mut action = compact_target_update_action(
            self.current_terminal_endpoint.as_ref(),
            self.panels[1]
                .requested_endpoint
                .as_ref()
                .unwrap_or(&self.panels[1].endpoint),
            endpoint.as_ref(),
            self.follow_terminal_directory || file_source,
        );
        // 本机终端保留手动目录；本机文件 Tab 则必须采用新文件的父目录。
        if file_source && action == CompactTargetUpdateAction::PreservePath {
            action = CompactTargetUpdateAction::FollowDirectory;
        }
        if binding_changed
            && (file_source
                || (self.follow_terminal_directory
                    && matches!(endpoint, Some(FileEndpoint::Remote { .. }))))
            && action == CompactTargetUpdateAction::Unchanged
        {
            action = CompactTargetUpdateAction::FollowDirectory;
        }
        if action == CompactTargetUpdateAction::Unavailable {
            if self.compact_target_is_cleared() {
                return;
            }
            self.current_terminal_endpoint = None;
            self.current_terminal_binding = None;
            self.external_target_lease = None;
            self.invalidate_compact_target(cx);
            cx.notify();
            return;
        }
        let Some(endpoint) = endpoint else {
            unreachable!("only a validated local or SSH endpoint can update the compact file rail")
        };
        let retained_lease = match &endpoint {
            FileEndpoint::Remote { host_id, .. } => {
                let current_lease = self
                    .external_target_lease
                    .as_ref()
                    .filter(|current| current.host_id() == host_id);
                match lease.as_ref() {
                    Some(lease) if lease.host_id() == host_id => match current_lease {
                        Some(current) => Some(current.clone()),
                        None => match lease.retain() {
                            Ok(lease) => Some(lease),
                            Err(error) => {
                                let message = localized_error(&error);
                                if !self.compact_target_error_is_current(&message) {
                                    self.current_terminal_endpoint = None;
                                    self.external_target_lease = None;
                                    self.invalidate_compact_target(cx);
                                    self.status = Some(message);
                                    cx.notify();
                                }
                                return;
                            }
                        },
                    },
                    Some(_) => {
                        self.reject_compact_target_lease(cx);
                        return;
                    }
                    None => current_lease.cloned(),
                }
            }
            FileEndpoint::Local { .. } if lease.is_none() => None,
            FileEndpoint::Local { .. } | FileEndpoint::Container { .. } => {
                self.reject_compact_target_lease(cx);
                return;
            }
        };

        let target_changed = self.current_terminal_endpoint.as_ref() != Some(&endpoint);
        let lease_changed = self.external_target_lease != retained_lease;
        if action == CompactTargetUpdateAction::Unchanged
            && !target_changed
            && !lease_changed
            && !binding_changed
        {
            return;
        }

        self.current_terminal_binding = binding;
        self.current_terminal_endpoint = Some(endpoint.clone());
        self.external_target_lease = retained_lease;

        // 文件系统切换清除旧目标；文件 Tab 跟随父目录，终端来源遵守本机保留和 SSH opt-in。
        let should_notify = match action {
            CompactTargetUpdateAction::BindTarget => {
                self.invalidate_compact_target(cx);
                self.panels[1].endpoint = endpoint.clone();
                self.panels[1].navigation_history.reset(endpoint.clone());
                self.open_endpoint_on_side(1, endpoint, cx);
                true
            }
            CompactTargetUpdateAction::FollowDirectory => {
                self.follow_terminal_directory_to(endpoint, cx);
                true
            }
            CompactTargetUpdateAction::PreservePath => lease_changed || binding_changed,
            CompactTargetUpdateAction::Unchanged => lease_changed || binding_changed,
            CompactTargetUpdateAction::Unavailable => false,
        };
        if should_notify {
            cx.notify();
        }
    }

    /// 验收只接受已绑定主机下精确目录的已提交 listing；导航仍由普通面板加载链执行。
    /// 只把该目录读取错误作为失败，传输取消等独立操作提示不能污染下一次目录就绪判断。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn right_rail_acceptance_ready(
        &self,
        host_id: &HostId,
        directory: &str,
    ) -> Result<bool, String> {
        let Some(FileEndpoint::Remote {
            host_id: terminal_host_id,
            ..
        }) = self.current_terminal_endpoint.as_ref()
        else {
            return Ok(false);
        };
        if terminal_host_id != host_id {
            return Err("The active terminal is bound to a different SSH host".to_owned());
        }

        let expected_directory = FileEndpoint::remote(host_id.clone(), directory)
            .map_err(|_| "The fixture SFTP directory path is invalid".to_owned())?;
        if !matches!(
            &expected_directory,
            FileEndpoint::Remote { path, .. } if path.as_str().starts_with('/')
        ) {
            return Err("The fixture SFTP directory path must be absolute".to_owned());
        }

        let panel = &self.panels[1];
        let FileEndpoint::Remote {
            host_id: panel_host_id,
            ..
        } = &panel.endpoint
        else {
            return Err("The SFTP pane is not bound to an SSH endpoint".to_owned());
        };
        if panel_host_id != host_id {
            return Err("The SFTP pane is bound to a different SSH host".to_owned());
        }
        if panel.loading || panel.requested_endpoint.is_some() {
            return Ok(false);
        }
        if let Some(error) = panel.directory_error.as_ref() {
            return Err(error.clone());
        }
        let Some(listing) = panel.listing.as_ref() else {
            return Ok(false);
        };
        let FileEndpoint::Remote {
            host_id: listing_host_id,
            ..
        } = &listing.directory
        else {
            return Err("The current file listing is not an SSH/SFTP listing".to_owned());
        };
        if listing_host_id != host_id {
            return Err("The current file listing belongs to a different SSH host".to_owned());
        }
        Ok(panel.endpoint == expected_directory && listing.directory == expected_directory)
    }

    /// 验收显式目录仅复用普通右侧导航与异步读取流程，并拒绝跨主机或相对路径。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn right_rail_acceptance_open_directory(
        &mut self,
        host_id: &HostId,
        directory: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(FileEndpoint::Remote {
            host_id: terminal_host_id,
            ..
        }) = self.current_terminal_endpoint.as_ref()
        else {
            return Err("The active terminal is not bound to an SSH host".to_owned());
        };
        if terminal_host_id != host_id {
            return Err("The active terminal is bound to a different SSH host".to_owned());
        }

        let endpoint = FileEndpoint::remote(host_id.clone(), directory)
            .map_err(|_| "The fixture SFTP directory path is invalid".to_owned())?;
        if !matches!(
            &endpoint,
            FileEndpoint::Remote { path, .. } if path.as_str().starts_with('/')
        ) {
            return Err("The fixture SFTP directory path must be absolute".to_owned());
        }
        let FileEndpoint::Remote {
            host_id: panel_host_id,
            ..
        } = &self.panels[1].endpoint
        else {
            return Err("The SFTP pane is not bound to an SSH endpoint".to_owned());
        };
        if panel_host_id != host_id {
            return Err("The SFTP pane is bound to a different SSH host".to_owned());
        }

        let panel = &self.panels[1];
        let needs_navigation = panel.endpoint != endpoint
            || (!panel.loading
                && panel
                    .listing
                    .as_ref()
                    .map(|listing| listing.directory != endpoint)
                    .unwrap_or(true));
        if needs_navigation {
            self.open_endpoint_on_side(1, endpoint, cx);
        }
        Ok(())
    }

    /// 将紧凑文件栏的显式信任动作交给 Root 复用连接层的 inspect/trust/readback 状态机。
    ///
    /// @author kongweiguang
    pub(crate) fn request_host_key_trust(&mut self, cx: &mut Context<Self>) {
        if !self.compact_layout || self.panels[1].directory_error.is_none() {
            return;
        }
        let endpoint = self.panels[1].endpoint.clone();
        let FileEndpoint::Remote { host_id, .. } = &endpoint else {
            return;
        };
        if self.pending_host_key_trust.as_ref().is_some_and(|pending| {
            pending.request.host_id == *host_id && pending.endpoint == endpoint
        }) {
            return;
        }
        self.next_host_key_trust_request_id =
            self.next_host_key_trust_request_id.wrapping_add(1).max(1);
        let request = SftpHostKeyTrustRequested {
            request_id: self.next_host_key_trust_request_id,
            host_id: host_id.clone(),
        };
        self.pending_host_key_trust = Some(PendingHostKeyTrust {
            request: request.clone(),
            endpoint,
            panel_generation: self.panels[1].generation,
        });
        cx.emit(request);
        cx.notify();
    }

    /// Root 按 request id 与 HostId 查询当前仍有效的 trust 请求，避免处理被新目标取代的事件。
    ///
    /// @author kongweiguang
    pub(crate) fn current_host_key_trust_request(&self) -> Option<SftpHostKeyTrustRequested> {
        self.pending_host_key_trust
            .as_ref()
            .filter(|pending| {
                self.panels[1].endpoint == pending.endpoint
                    && self.panels[1].generation == pending.panel_generation
            })
            .map(|pending| pending.request.clone())
    }

    /// 仅接受当前请求回执；成功后只重载发起时仍在显示的目录，不导航或重绑目标。
    ///
    /// @author kongweiguang
    pub(crate) fn report_host_key_trust(
        &mut self,
        request_id: u64,
        host_id: HostId,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.pending_host_key_trust.as_ref() else {
            return;
        };
        if current.request.request_id != request_id || current.request.host_id != host_id {
            return;
        }
        let Some(pending) = self.pending_host_key_trust.take() else {
            return;
        };
        let current_target_matches = self.panels[1].endpoint == pending.endpoint
            && self.panels[1].generation == pending.panel_generation
            && self.panels[1].endpoint.host_id() == Some(&host_id);
        if !current_target_matches {
            cx.notify();
            return;
        }
        match result {
            Ok(()) => {
                self.status = None;
                self.load_panel_at(1, pending.endpoint, cx);
            }
            Err(error) => self.status = Some(error),
        }
        cx.notify();
    }

    /// 清除旧文件系统的清单和表单、取消根 provider，并推进代次拒收迟到回执。
    /// 已提交名称、权限和删除操作继续持有 busy 到真实回执；撤销展示不能假装后台操作已结束。
    ///
    /// @author kongweiguang
    fn invalidate_compact_target(&mut self, cx: &mut Context<Self>) {
        self.terminal_directory_follow_generation = None;
        self.directory_navigation_generations[1] =
            self.directory_navigation_generations[1].wrapping_add(1);
        let panel = &mut self.panels[1];
        panel.cancel_root_request_cancellation();
        panel.generation = panel.generation.wrapping_add(1);
        panel.loading = false;
        panel.directory_error = None;
        panel.failed_directory = None;
        panel.requested_endpoint = None;
        panel.listing = None;
        panel.selected = None;
        panel.selection.clear();
        panel.keyboard_focus.clear();
        panel.pending_selection = None;
        panel.tree.cancel_pending_requests();
        self.bookmark_generation = self.bookmark_generation.wrapping_add(1);
        self.bookmarks.clear();
        self.bookmarks_target = None;
        self.bookmark_request_target = None;
        self.bookmark_error = None;
        self.bookmark_error_target = None;
        self.pending_host_key_trust = None;
        self.delete_confirmation = None;
        self.name_action = None;
        self.name_action_side = None;
        self.name_action_target = None;
        self.permission_dialog_target = None;
        self.permissions.reset_selection();
        self.status = None;
        self.cancel_transfer_preparation_for_side(1, cx);
    }

    /// 避免 Root 每次 render 都重复清理已失效的文件目标和推进请求代次。
    ///
    /// @author kongweiguang
    fn compact_target_is_cleared(&self) -> bool {
        let panel = &self.panels[1];
        self.current_terminal_endpoint.is_none()
            && self.external_target_lease.is_none()
            && !panel.loading
            && panel.directory_error.is_none()
            && panel.listing.is_none()
            && panel.selected.is_none()
            && panel.selection == selection::FileSelection::default()
            && panel.pending_selection.is_none()
            && self.bookmarks.is_empty()
            && self.bookmarks_target.is_none()
            && self.bookmark_request_target.is_none()
            && self.bookmark_error.is_none()
            && self.bookmark_error_target.is_none()
            && self.pending_host_key_trust.is_none()
            && self.delete_confirmation.is_none()
            && self.name_action.is_none()
            && self.permission_dialog_target.is_none()
            && self.status.is_none()
    }

    /// 重复收到同一失效租约时保留首次错误，但不反复失效请求或通知重绘。
    ///
    /// @author kongweiguang
    fn compact_target_error_is_current(&self, message: &str) -> bool {
        self.current_terminal_endpoint.is_none()
            && self.external_target_lease.is_none()
            && self.panels[1].listing.is_none()
            && !self.panels[1].loading
            && self.status.as_deref() == Some(message)
    }

    /// 无法把临时 SSH lease 绑定到当前 endpoint 时撤销旧目标并保留就地诊断。
    ///
    /// @author kongweiguang
    fn reject_compact_target_lease(&mut self, cx: &mut Context<Self>) {
        let message = locale::text(
            "当前文件目标与连接租约不匹配。",
            "The file target does not match its connection lease.",
        )
        .to_string();
        if self.compact_target_error_is_current(&message) {
            return;
        }
        self.current_terminal_endpoint = None;
        self.external_target_lease = None;
        self.invalidate_compact_target(cx);
        self.status = Some(message);
        cx.notify();
    }

    /// 紧凑文件栏仅在浏览器仍属于 Root 当前精确终端文件系统时可操作。
    ///
    /// @author kongweiguang
    pub(super) fn compact_target_is_available(&self) -> bool {
        self.current_terminal_endpoint
            .as_ref()
            .is_some_and(|current| endpoint_same_filesystem(current, &self.panels[1].endpoint))
    }

    /// 已弹出的行菜单只对创建它时的面板代次生效，防止切主机后迟到点击写入新目标。
    ///
    /// @author kongweiguang
    pub(super) fn browser_action_snapshot_is_current(
        &self,
        side: usize,
        generation: u64,
        endpoint: &FileEndpoint,
    ) -> bool {
        let Some(panel) = self.panels.get(side) else {
            return false;
        };
        panel.generation == generation
            && &panel.endpoint == endpoint
            && (!self.compact_layout || self.compact_target_is_available())
    }

    /// 写入回执绑定导航身份，同目录刷新不使其失效；离开再返回同一路径仍拒绝旧回执。
    ///
    /// @author kongweiguang
    fn file_action_directory_is_current(
        &self,
        side: usize,
        generation: u64,
        directory: &FileEndpoint,
    ) -> bool {
        self.panels.get(side).is_some_and(|panel| {
            panel.endpoint == *directory
                && self.directory_navigation_generations[side] == generation
                && panel
                    .requested_endpoint
                    .as_ref()
                    .is_none_or(|requested| requested == directory)
        })
    }

    /// 读取错误保留在目录 owner，渲染按当前侧派生；业务反馈无需依靠翻译后的文本猜测来源。
    ///
    /// @author kongweiguang
    fn visible_operation_status(&self) -> Option<&str> {
        if let Some(status) = self.status.as_deref() {
            return Some(status);
        }
        // 列表已用内联错误卡展示同一失败时底栏不再复述，重试只出现一次；无法渲染列表
        // （未绑定终端目标）或保留旧清单时，底栏仍补位并提供原位重试。
        if self.directory_error_is_inline() {
            return None;
        }
        let side = if self.compact_layout {
            1
        } else {
            self.active_side
        };
        self.panels[side].directory_error.as_deref()
    }

    /// 目录错误是否已由列表内联卡片展示；与 `render_browser` 的错误卡判定保持同一条件。
    ///
    /// @author kongweiguang
    fn directory_error_is_inline(&self) -> bool {
        let side = if self.compact_layout {
            1
        } else {
            self.active_side
        };
        let panel = &self.panels[side];
        if panel.directory_error.is_none() || panel.listing.is_some() {
            return false;
        }
        if self.compact_layout {
            self.compact_target_is_available()
        } else {
            !(self.host_picker.enabled && self.host_picker.target_unselected[side])
        }
    }

    /// 新导航只撤销该侧的旧表单与回执展示资格；在途任务继续独占 busy，直到真实回执收尾。
    ///
    /// @author kongweiguang
    fn invalidate_file_action_presentation(&mut self, side: usize) {
        self.directory_navigation_generations[side] =
            self.directory_navigation_generations[side].wrapping_add(1);
        self.status = None;
        self.status_details_expanded = false;
        if self.name_action_side == Some(side) {
            self.name_action = None;
            self.name_action_side = None;
            self.name_action_target = None;
        }
        if self
            .permission_dialog_target
            .as_ref()
            .is_some_and(|(owner, _)| *owner == side)
        {
            self.permission_dialog_target = None;
            self.permissions.reset_selection();
        }
        self.delete_confirmation = None;
    }

    /// 切换语言时只更新已有输入的提示，不重建实体或覆盖用户正在编辑的路径和名称草稿。
    ///
    /// @author kongweiguang
    pub fn refresh_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for panel in &self.panels {
            if let Some(input) = panel.path_input.as_ref() {
                input.update(cx, |state, cx| {
                    state.set_placeholder(
                        locale::text("输入目录路径", "Enter a directory path"),
                        window,
                        cx,
                    )
                });
            }
        }
        if let Some(input) = self.name_input.as_ref() {
            input.update(cx, |state, cx| {
                state.set_placeholder(
                    locale::text("新文件夹名称 / 重命名", "Folder name / rename"),
                    window,
                    cx,
                )
            });
        }
        self.host_picker.refresh_locale(window, cx);
        if let Some(input) = self.permissions.input.as_ref() {
            input.update(cx, |state, cx| {
                state.set_placeholder(
                    locale::text("八进制权限，例如 0755", "Octal mode, e.g. 0755"),
                    window,
                    cx,
                )
            });
        }
        cx.notify();
    }

    /// 切换列表与惰性树只同步可见选择，不触发额外 IO 或选择裁剪提示；计数直接反映结果。
    ///
    /// @author kongweiguang
    pub(super) fn toggle_tree_view(&mut self, cx: &mut Context<Self>) {
        let side = self.active_side;
        if self.view_mode == view_mode::FilesViewMode::List {
            let endpoints = self.panels[side]
                .listing
                .as_ref()
                .map(|listing| {
                    listing
                        .entries
                        .iter()
                        .filter(|entry| {
                            self.panels[side].selection.contains(&entry.name)
                                && (self.show_hidden_files
                                    || !render::browser::is_hidden_entry(entry))
                        })
                        .map(|entry| entry.endpoint.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            self.panels[side].tree.select_endpoints(endpoints);
            self.view_mode = view_mode::FilesViewMode::Tree;
        } else {
            let root = self.panels[side].endpoint.clone();
            let selected = self.panels[side]
                .tree
                .selected_entries(self.show_hidden_files);
            let root_selected = selected
                .iter()
                .filter(|entry| parent_endpoint(&entry.endpoint).as_ref() == Some(&root))
                .cloned()
                .collect::<Vec<_>>();
            let selected_names = root_selected
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>();
            self.panels[side].selection.clear();
            self.panels[side].selection.select_all(&selected_names);
            self.panels[side].selected = root_selected.first().cloned();
            self.view_mode = view_mode::FilesViewMode::List;
        }
        self.delete_confirmation = None;
        cx.notify();
    }

    /// 将视图菜单项映射到列表与惰性树切换。
    ///
    /// @author kongweiguang
    fn set_file_view_mode(&mut self, target: view_mode::FilesViewMode, cx: &mut Context<Self>) {
        if self.view_mode != target {
            self.toggle_tree_view(cx);
        }
    }

    /// 只在 Tree 首次显示或模型版本变化时更新 Kit 状态，保留其虚拟列表和键盘焦点。
    ///
    /// @author kongweiguang
    pub(super) fn ensure_tree_widget(&mut self, side: usize, cx: &mut Context<Self>) {
        if self.panels[side].tree_widget.is_none() {
            let projection = self.panels[side].tree.projection(self.show_hidden_files);
            let state = cx.new(|cx| TreeState::new(cx).items(projection.items));
            self.panels[side].tree_widget = Some(tree::TreeWidget::new(side, state, cx));
        }

        let projection = self.panels[side].tree.projection(self.show_hidden_files);
        let Some(widget) = self.panels[side].tree_widget.as_mut() else {
            return;
        };
        if widget.projected_revision == projection.revision {
            return;
        }

        widget.node_map = projection.nodes;
        widget.projected_revision = projection.revision;
        let state = widget.state.clone();
        let focused_id = projection.focused_id.map(gpui_kit::SharedString::from);
        state.update(cx, |state, cx| {
            state.set_items(projection.items, cx);
            let selected_index = focused_id.as_ref().and_then(|id| state.index_of(id));
            if selected_index.is_some() {
                state.set_selected_index(selected_index, cx);
            }
        });
    }

    /// 将 GPUI Tree 的展开与折叠事件投影到端点级缓存，并在展开时只请求一层子目录。
    ///
    /// @author kongweiguang
    fn handle_tree_event(
        &mut self,
        side: usize,
        _state: Entity<TreeState>,
        event: TreeEvent,
        cx: &mut Context<Self>,
    ) {
        if self.panels[side].loading {
            return;
        }
        let node_id = match &event {
            TreeEvent::Expanded(id) | TreeEvent::Collapsed(id) => id.as_ref(),
        };
        let node = self.panels[side]
            .tree_widget
            .as_ref()
            .and_then(|widget| widget.node_map.nodes.get(node_id))
            .cloned();
        let Some(tree::TreeNode::Entry(endpoint)) = node else {
            return;
        };

        match event {
            TreeEvent::Expanded(_) => {
                self.panels[side].tree.expand(&endpoint);
                self.load_tree_directory(side, endpoint, cx);
            }
            TreeEvent::Collapsed(_) => {
                self.panels[side].tree.collapse(&endpoint);
                if let Some(entry) = self.panels[side].tree.entry(&endpoint)
                    && self.panels[side].selected.as_ref().is_some_and(|selected| {
                        parent_endpoint(&selected.endpoint).as_ref() == Some(&endpoint)
                            || tree::is_descendant_endpoint(&selected.endpoint, &endpoint)
                    })
                {
                    self.panels[side].selected = Some(entry);
                }
            }
        }
        cx.notify();
    }

    /// GPUI 的焦点行只同步单项选择；批量选择仍由完整 endpoint 集合单独维护。
    ///
    /// @author kongweiguang
    fn handle_tree_focus_changed(
        &mut self,
        side: usize,
        state: Entity<TreeState>,
        cx: &mut Context<Self>,
    ) {
        let Some(node_id) = state
            .read(cx)
            .selected_item()
            .map(|item| item.id.to_string())
        else {
            return;
        };
        let node = self.panels[side]
            .tree_widget
            .as_ref()
            .and_then(|widget| widget.node_map.nodes.get(&node_id))
            .cloned();
        let Some(tree::TreeNode::Entry(endpoint)) = node else {
            return;
        };
        let Some(entry) = self.panels[side].tree.entry(&endpoint) else {
            return;
        };
        if side == 0 && !endpoint_matches_locked_host(&endpoint, self.locked_left_host_id.as_ref())
        {
            self.status = Some(
                locale::text(
                    "文件项不属于左侧锁定的主机。",
                    "The file entry is outside the host locked to the left panel.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        self.panels[side].tree.set_focused(Some(endpoint.clone()));
        if self.active_side != side {
            self.activate_side(side, cx);
        }
        if self.panels[side]
            .selected
            .as_ref()
            .is_some_and(|selected| selected.endpoint == endpoint)
        {
            return;
        }
        self.panels[side].selected = Some(entry.clone());
        cx.notify();
    }

    /// 子目录读取受 executor 监督，token、完整 endpoint、面板代次和请求 id 必须同时匹配。
    ///
    /// @author kongweiguang
    fn load_tree_directory(&mut self, side: usize, endpoint: FileEndpoint, cx: &mut Context<Self>) {
        if self.panels[side].loading {
            return;
        }
        let generation = self.panels[side].generation;
        let Some((request_id, cancellation)) = self.panels[side]
            .tree
            .begin_directory_request(&endpoint, generation)
        else {
            cx.notify();
            return;
        };
        let external_target_lease = self.external_target_lease_for_endpoint(&endpoint);
        let files = self.files.clone();
        let executor = self.executor.clone();
        let request_endpoint = endpoint.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    cancellation.bind_task_token(cancel.clone());
                    let _external_target_scope = external_target_lease;
                    files.list_directory(cancel, request_endpoint).await
                })
                .await
                .map_err(|error| localized_error(&error));
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    if pane.panels[side].generation != generation {
                        return;
                    }
                    if side == 0
                        && result.as_ref().is_ok_and(|listing| {
                            listing.entries.iter().any(|entry| {
                                !endpoint_matches_locked_host(
                                    &entry.endpoint,
                                    pane.locked_left_host_id.as_ref(),
                                )
                            })
                        })
                    {
                        let _ = pane.panels[side].tree.complete_directory_request(
                            &endpoint,
                            generation,
                            request_id,
                            pane.show_hidden_files,
                            Err(locale::text(
                                "子目录包含锁定主机之外的路径",
                                "The directory contains a path outside the locked host",
                            )
                            .to_string()),
                        );
                        cx.notify();
                        return;
                    }
                    let completed = pane.panels[side].tree.complete_directory_request(
                        &endpoint,
                        generation,
                        request_id,
                        pane.show_hidden_files,
                        result,
                    );
                    if completed {
                        cx.notify();
                    }
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 树选择只供本模块及其渲染子模块使用，不向其它 feature 暴露内部选择手势类型。
    ///
    /// @author kongweiguang
    fn select_tree_entry(
        &mut self,
        side: usize,
        endpoint: FileEndpoint,
        gesture: selection::SelectionGesture,
        cx: &mut Context<Self>,
    ) {
        if side == 0 && !endpoint_matches_locked_host(&endpoint, self.locked_left_host_id.as_ref())
        {
            self.status = Some(
                locale::text(
                    "文件项不属于左侧锁定的主机。",
                    "The file entry is outside the host locked to the left panel.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if self.active_side != side {
            self.activate_side(side, cx);
        }
        let visible = self.panels[side]
            .tree
            .visible_endpoints(self.show_hidden_files);
        self.panels[side].tree.set_focused(Some(endpoint.clone()));
        self.panels[side]
            .tree
            .apply_selection(&endpoint, &visible, gesture);
        self.delete_confirmation = None;
        let selected = self.panels[side]
            .tree
            .selected_entries(self.show_hidden_files);
        let focused = self.panels[side]
            .tree
            .entry(&endpoint)
            .filter(|_| self.panels[side].tree.is_selected(&endpoint))
            .or_else(|| selected.first().cloned());
        self.panels[side].selected = focused;
        cx.notify();
    }

    /// 复选框仅切换单个完整 endpoint；随后把同一节点同步给 Kit Tree 并恢复树键盘焦点。
    ///
    /// @author kongweiguang
    pub(super) fn toggle_tree_checkbox(
        &mut self,
        side: usize,
        endpoint: FileEndpoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_tree_entry(side, endpoint, selection::SelectionGesture::Toggle, cx);
        self.ensure_tree_widget(side, cx);
        if let Some(state) = self.panels[side]
            .tree_widget
            .as_ref()
            .map(|widget| widget.state.clone())
        {
            state.update(cx, |state, cx| state.focus(window, cx));
        }
    }

    /// 外部连接入口将文件面板切换到明确提供的 endpoint，不推断当前终端或其它主机。
    ///
    /// @author kongweiguang
    pub fn open_endpoint(&mut self, endpoint: FileEndpoint, cx: &mut Context<Self>) {
        let side = if matches!(endpoint, FileEndpoint::Local { .. }) {
            0
        } else {
            1
        };
        self.open_endpoint_on_side(side, endpoint, cx);
    }

    /// 主机默认目录仅用于首次绑定；共享工作台延迟选主机时只在左侧仍处初始路径时应用本机默认目录。
    /// 同一远端再次打开保留用户位置，恢复或显式导航优先于默认值。
    /// 本机起点重绑也撤销旧写入的展示资格，不释放在途请求的 busy。
    ///
    /// @author kongweiguang
    pub(crate) fn open_host_endpoint(
        &mut self,
        endpoint: FileEndpoint,
        initial_local_directory: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let apply_shared_local_start = self.initialized
            && should_apply_shared_host_local_start(
                self.host_picker.enabled,
                self.host_picker.target_unselected[1],
                &self.panels[0].endpoint,
                self.panels[0].navigation_history.is_initial(),
                self.locked_left_host_id.is_some(),
            );
        self.cancel_pending_host_selection(FilePaneSide::Right, cx);
        self.host_picker.target_unselected[1] = false;
        self.host_picker.expanded_side = None;
        if (self.initialized || self.panels[1].generation > 0)
            && endpoint_same_filesystem(&self.panels[1].endpoint, &endpoint)
        {
            if !self.initialized {
                self.initialize(cx);
            }
            self.activate_side(1, cx);
            self.reload_transfers(cx);
            return;
        }
        if !self.initialized {
            if let Some(path) = initial_local_directory
                && self.panels[0].generation == 0
                && self.locked_left_host_id.is_none()
            {
                self.invalidate_file_action_presentation(0);
                self.panels[0].endpoint = FileEndpoint::Local { path };
            }
            self.initialized = true;
            self.sync_target_name_requests(cx);
            // 无效初始目录沿原读取错误展示，不静默回退主目录或其它位置。
            self.load_panel(0, cx);
            self.reload_transfers(cx);
        } else if apply_shared_local_start && let Some(path) = initial_local_directory {
            let local_endpoint = FileEndpoint::Local { path };
            if self.panels[0].endpoint != local_endpoint {
                self.invalidate_file_action_presentation(0);
                self.panels[0].endpoint = local_endpoint.clone();
                self.panels[0]
                    .navigation_history
                    .reset(local_endpoint.clone());
                self.panels[0].listing = None;
                self.panels[0].selected = None;
                self.panels[0].selection.clear();
                self.panels[0].tree.cancel_pending_requests();
                self.load_panel_at(0, local_endpoint, cx);
            }
        }
        if self.host_picker.enabled {
            self.bind_selected_endpoint(FilePaneSide::Right, endpoint, cx);
        } else {
            self.open_endpoint(endpoint, cx);
        }
    }

    /// 切换活动面板时复用目标级书签缓存。
    ///
    /// @author kongweiguang
    fn activate_side(&mut self, side: usize, cx: &mut Context<Self>) {
        if self.active_side == side {
            return;
        }
        self.active_side = side;
        self.load_bookmarks(side, cx);
        cx.notify();
    }

    /// 将 endpoint 放入指定面板；工作区锁定只约束左侧，不影响右侧复制目标。
    ///
    /// @author kongweiguang
    fn open_endpoint_on_side(
        &mut self,
        side: usize,
        endpoint: FileEndpoint,
        cx: &mut Context<Self>,
    ) {
        self.open_endpoint_on_side_with_selection(side, endpoint, None, cx);
    }

    /// 将 endpoint 与待选策略一次写入面板，再发起目录读取，避免响应快于选择设置。
    ///
    /// @author kongweiguang
    fn open_endpoint_on_side_with_selection(
        &mut self,
        side: usize,
        endpoint: FileEndpoint,
        pending_selection: Option<PendingSelection>,
        cx: &mut Context<Self>,
    ) {
        if self.host_picker.enabled && side < self.panels.len() {
            if self.host_picker.target_pending[side]
                && self.panels[side].endpoint != endpoint
                && let Some(side) = FilePaneSide::from_index(side)
            {
                self.cancel_pending_host_selection(side, cx);
            }
            self.host_picker.target_unselected[side] = false;
            self.host_picker.expanded_side = None;
        }
        if side == 0 && !endpoint_matches_locked_host(&endpoint, self.locked_left_host_id.as_ref())
        {
            self.status = Some(
                locale::text(
                    "目标不属于左侧锁定的主机。",
                    "The target is outside the host locked to the left panel.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        self.active_side = side;
        self.panels[side].pending_selection = pending_selection;
        self.panels[side].navigation_history.push(endpoint.clone());
        self.delete_confirmation = None;
        self.load_panel_at(side, endpoint, cx);
        cx.notify();
    }

    /// 首次绘制时创建路径输入，并只在 endpoint 真正变化后同步其值，保留未提交草稿。
    ///
    /// @author kongweiguang
    fn ensure_path_input(
        &mut self,
        side: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let current_path = render::browser::endpoint_path(&self.panels[side].endpoint);
        if let Some(input) = self.panels[side].path_input.as_ref().cloned() {
            if self.panels[side].path_input_endpoint.as_deref() != Some(current_path.as_str()) {
                self.panels[side].path_input_endpoint = Some(current_path.clone());
                input.update(cx, |state, cx| state.set_value(current_path, window, cx));
            }
            return input;
        }

        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(current_path.clone())
                .placeholder(locale::text("输入目录路径", "Enter a directory path"))
        });
        let subscription = cx.subscribe(&input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let panel = &mut self.panels[side];
        panel.path_input = Some(input.clone());
        panel.path_input_endpoint = Some(current_path);
        panel._path_subscription = Some(subscription);
        input
    }

    /// 用当前已打开目录覆盖路径草稿；取消不发起 IO，也不改变当前列表。
    ///
    /// @author kongweiguang
    fn cancel_path_edit(&mut self, side: usize, window: &mut Window, cx: &mut Context<Self>) {
        let current_path = render::browser::endpoint_path(&self.panels[side].endpoint);
        if let Some(input) = self.panels[side].path_input.as_ref() {
            input.update(cx, |state, cx| {
                state.set_value(current_path.clone(), window, cx)
            });
        }
        self.panels[side].path_input_endpoint = Some(current_path);
        self.status = None;
        cx.notify();
    }

    /// Enter 和跳转按钮共用路径提交；输入法组合未结束时保留草稿与目录，避免提交未确认文本。
    ///
    /// @author kongweiguang
    fn commit_path_edit(&mut self, side: usize, cx: &mut Context<Self>) {
        let Some(input) = self.panels[side].path_input.as_ref().cloned() else {
            return;
        };
        if input.read(cx).is_composing() {
            return;
        }
        let draft = input.update(cx, |state, _| state.value().to_string());
        let endpoint = match render::browser::endpoint_for_path(&self.panels[side].endpoint, &draft)
        {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.status = Some(localized_error(&error));
                cx.notify();
                return;
            }
        };
        if side == 0 && !endpoint_matches_locked_host(&endpoint, self.locked_left_host_id.as_ref())
        {
            self.status = Some(
                locale::text(
                    "目标不属于左侧锁定的主机。",
                    "The target is outside the host locked to the left panel.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }

        self.active_side = side;
        self.panels[side].pending_selection = None;
        self.panels[side].selected = None;
        self.panels[side].selection.clear();
        self.delete_confirmation = None;
        self.status = None;
        self.panels[side].navigation_history.push(endpoint.clone());
        self.load_panel_at(side, endpoint, cx);
        cx.notify();
    }

    /// 隐藏文件同时裁剪不可见选择，结果由行和计数表达，不额外插入选择裁剪提示。
    ///
    /// @author kongweiguang
    fn toggle_hidden_files(&mut self, cx: &mut Context<Self>) {
        self.show_hidden_files = !self.show_hidden_files;
        for panel in &mut self.panels {
            panel.tree.retain_visible_selection(self.show_hidden_files);
        }
        if !self.show_hidden_files {
            for side in 0..self.panels.len() {
                let visible_names = self.visible_names(side);
                self.panels[side].selection.retain_visible(&visible_names);
                let hidden_selection = self.panels[side]
                    .selected
                    .as_ref()
                    .is_some_and(render::browser::is_hidden_entry);
                if hidden_selection {
                    self.panels[side].selected = None;
                }
            }
        }
        cx.notify();
    }

    /// 按当前可见且已排序的目录投影返回名称，范围选择与绘制共享同一顺序。
    ///
    /// @author kongweiguang
    fn visible_names(&self, side: usize) -> Vec<String> {
        let Some(listing) = self.panels[side].listing.as_ref() else {
            return Vec::new();
        };
        let mut entries = listing
            .entries
            .iter()
            .filter(|entry| self.show_hidden_files || !render::browser::is_hidden_entry(entry))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            render::browser::compare_entries(left, right, self.panels[side].sort)
        });
        entries
            .into_iter()
            .map(|entry| entry.name.clone())
            .collect()
    }

    /// 从当前列表投影出仍然存在的多选项；旧恢复入口的单项选择保留兼容回退。
    ///
    /// @author kongweiguang
    fn selected_entries(&self, side: usize) -> Vec<FileEntry> {
        let panel = &self.panels[side];
        if self.view_mode == view_mode::FilesViewMode::Tree {
            return panel.tree.selected_entries(self.show_hidden_files);
        }
        let Some(listing) = panel.listing.as_ref() else {
            return panel.selected.clone().into_iter().collect();
        };
        let mut entries = listing
            .entries
            .iter()
            .filter(|entry| {
                (self.show_hidden_files || !render::browser::is_hidden_entry(entry))
                    && panel.selection.contains(&entry.name)
            })
            .cloned()
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| render::browser::compare_entries(left, right, panel.sort));
        entries
    }

    /// 更新选择并绑定稳定身份；切换侧别时同时加载该侧书签。
    ///
    /// @author kongweiguang
    fn select_entry(
        &mut self,
        side: usize,
        entry: FileEntry,
        gesture: selection::SelectionGesture,
        ordered_names: &[String],
        cx: &mut Context<Self>,
    ) {
        if side == 0
            && !endpoint_matches_locked_host(&entry.endpoint, self.locked_left_host_id.as_ref())
        {
            self.status = Some(
                locale::text(
                    "文件项不属于左侧锁定的主机。",
                    "The file entry is outside the host locked to the left panel.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if self.active_side != side {
            self.active_side = side;
            self.load_bookmarks(side, cx);
        }
        self.delete_confirmation = None;
        let directory = self.panels[side].endpoint.clone();
        self.panels[side]
            .keyboard_focus
            .set_active(&directory, &entry.name, ordered_names);
        self.panels[side]
            .selection
            .apply(&entry.name, ordered_names, gesture);
        let focused_entry = if self.panels[side].selection.contains(&entry.name) {
            Some(entry)
        } else {
            self.panels[side]
                .listing
                .as_ref()
                .and_then(|listing| {
                    listing.entries.iter().find(|candidate| {
                        self.panels[side].selection.contains(&candidate.name)
                            && (self.show_hidden_files
                                || !render::browser::is_hidden_entry(candidate))
                    })
                })
                .cloned()
        };
        self.panels[side].selected = focused_entry;
        cx.notify();
    }

    /// 鼠标行不保留独立焦点，点击后回到列表 owner 以便箭头键跨虚拟行持续工作。
    ///
    /// @author kongweiguang
    fn focus_browser_list(&self, side: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handle) = self
            .panels
            .get(side)
            .and_then(|panel| panel.list_focus_handle.as_ref())
        {
            window.focus(handle, cx);
        }
    }

    /// 校验目标后按当前 viewport 移动，并直接滚动到活动行；行高沿用触发事件的绘制帧，避免文字缩放后翻页错位。
    ///
    /// @author kongweiguang
    fn move_list_keyboard_focus(
        &mut self,
        navigation: FileListNavigation<'_>,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) {
        let FileListNavigation {
            side,
            generation,
            directory,
            ordered_names,
            row_height,
        } = navigation;
        let movement = match event.keystroke.key.to_ascii_lowercase().as_str() {
            "up" => FileListMovement::Previous,
            "down" => FileListMovement::Next,
            "pageup" => FileListMovement::PagePrevious {
                visible_rows: self.panels.get(side).map_or(0, |panel| {
                    (f32::from(panel.list_scroll.bounds().size.height) / row_height).floor()
                        as usize
                }),
            },
            "pagedown" => FileListMovement::PageNext {
                visible_rows: self.panels.get(side).map_or(0, |panel| {
                    (f32::from(panel.list_scroll.bounds().size.height) / row_height).floor()
                        as usize
                }),
            },
            "home" => FileListMovement::First,
            "end" => FileListMovement::Last,
            _ => return,
        };
        if !self.browser_action_snapshot_is_current(side, generation, directory) {
            return;
        }
        let Some(panel) = self.panels.get_mut(side) else {
            return;
        };
        if panel.loading {
            return;
        }
        let Some(listing) = panel
            .listing
            .as_ref()
            .filter(|listing| &listing.directory == directory)
        else {
            return;
        };
        let Some(index) = panel
            .keyboard_focus
            .move_active(directory, ordered_names, movement)
        else {
            return;
        };
        let Some(name) = ordered_names.get(index) else {
            return;
        };
        let Some(entry) = listing
            .entries
            .iter()
            .find(|entry| &entry.name == name)
            .cloned()
        else {
            return;
        };
        let scroll = panel.list_scroll.clone();

        self.select_entry(
            side,
            entry,
            if event.keystroke.modifiers.shift {
                selection::SelectionGesture::Extend
            } else {
                selection::SelectionGesture::Replace
            },
            ordered_names,
            cx,
        );
        scroll.scroll_to_item(index, gpui_kit::ScrollStrategy::Nearest);
    }

    /// 键盘操作只读取当前 generation 的可见活动项，避免回收行或迟到事件操作旧目录。
    ///
    /// @author kongweiguang
    fn keyboard_active_entry(
        &self,
        side: usize,
        generation: u64,
        directory: &FileEndpoint,
        ordered_names: &[String],
    ) -> Option<FileEntry> {
        if !self.browser_action_snapshot_is_current(side, generation, directory) {
            return None;
        }
        let panel = self.panels.get(side)?;
        if panel.loading {
            return None;
        }
        let listing = panel
            .listing
            .as_ref()
            .filter(|listing| &listing.directory == directory)?;
        let active_name = panel.keyboard_focus.active_name(directory, ordered_names)?;
        listing
            .entries
            .iter()
            .find(|entry| entry.name.as_str() == active_name)
            .cloned()
    }

    /// 对活动目录执行全选或清空；全选严格作用于当前过滤后的可见行。
    ///
    /// @author kongweiguang
    fn toggle_select_all(&mut self, side: usize, cx: &mut Context<Self>) {
        self.activate_side(side, cx);
        if self.view_mode == view_mode::FilesViewMode::Tree {
            let visible = self.panels[side]
                .tree
                .visible_endpoints(self.show_hidden_files);
            if visible.is_empty() {
                self.status = Some(
                    locale::text(
                        "当前树视图没有已加载且可见的项目。",
                        "There are no loaded visible items in the tree.",
                    )
                    .to_string(),
                );
                cx.notify();
                return;
            }
            self.panels[side].tree.toggle_select_all(&visible);
            let selected = self.panels[side]
                .tree
                .selected_entries(self.show_hidden_files);
            self.panels[side].selected = selected.first().cloned();
            self.delete_confirmation = None;
            cx.notify();
            return;
        }
        let visible_names = self.visible_names(side);
        if visible_names.is_empty() {
            self.status = Some(
                locale::text(
                    "当前目录没有可选择的项目。",
                    "There are no visible items to select.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let all_selected = visible_names
            .iter()
            .all(|name| self.panels[side].selection.contains(name));
        if all_selected {
            self.panels[side].selection.clear();
            self.panels[side].selected = None;
        } else {
            self.panels[side].selection.select_all(&visible_names);
            if let Some(entry) = self.panels[side]
                .listing
                .as_ref()
                .and_then(|listing| {
                    listing.entries.iter().find(|entry| {
                        self.panels[side].selection.contains(&entry.name)
                            && (self.show_hidden_files || !render::browser::is_hidden_entry(entry))
                    })
                })
                .cloned()
            {
                self.panels[side].selected = Some(entry);
            }
        }
        cx.notify();
    }

    /// Enter 与双击共用文件打开门禁，临时目标必须在离开源面板前取得独立租约。
    ///
    /// @author kongweiguang
    fn open_entry(&mut self, side: usize, entry: FileEntry, cx: &mut Context<Self>) {
        if entry.kind == FileEntryKind::Directory {
            self.activate_entry(side, entry, cx);
            return;
        }
        if entry.kind != FileEntryKind::File {
            self.status = Some(
                locale::text(
                    "只有普通文件可以在编辑器中打开。",
                    "Only regular files can be opened in the editor.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        self.activate_side(side, cx);
        self.panels[side].selection.select_only(
            endpoint_name(&entry.endpoint)
                .as_deref()
                .unwrap_or(entry.name.as_str()),
        );
        if self.view_mode == view_mode::FilesViewMode::Tree {
            self.panels[side].tree.select_only(&entry.endpoint);
        }
        self.panels[side].selected = Some(entry.clone());
        self.request_open_file(entry.endpoint, cx);
        cx.notify();
    }

    /// 所有文件入口共用租约检查；目标已失效时保留原面板并显示错误，不改投当前主机。
    ///
    /// @author kongweiguang
    fn request_open_file(&mut self, endpoint: FileEndpoint, cx: &mut Context<Self>) {
        let external_target_lease =
            match retain_file_target_lease(&endpoint, self.external_target_lease.as_ref()) {
                Ok(lease) => lease,
                Err(error) => {
                    self.status = Some(
                        locale::text(
                            error.public_message(),
                            "The temporary connection is unavailable; the file cannot be opened.",
                        )
                        .to_string(),
                    );
                    cx.notify();
                    return;
                }
            };
        cx.emit(OpenFileRequested {
            endpoint,
            external_target_lease,
        });
    }

    /// 打开传输目标的父目录；目标可能尚未发布，只有存在时才选择它。
    ///
    /// @author kongweiguang
    pub(super) fn locate_transfer_destination(
        &mut self,
        destination: FileEndpoint,
        cx: &mut Context<Self>,
    ) {
        let Some(parent) = parent_endpoint(&destination) else {
            self.status = Some(
                locale::text(
                    "目标路径没有可显示的父目录。",
                    "The destination has no parent directory to display.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let side = if endpoint_same_filesystem(&self.panels[0].endpoint, &destination) {
            0
        } else if endpoint_same_filesystem(&self.panels[1].endpoint, &destination) {
            1
        } else if matches!(parent, FileEndpoint::Local { .. }) {
            0
        } else {
            1
        };
        let selection = endpoint_name(&destination).map(PendingSelection::IfPresent);
        self.open_endpoint_on_side_with_selection(side, parent, selection, cx);
    }

    /// 恢复一个独立的双面板传输工作区，并校验左侧锁定主机与初始选择。
    /// 两侧重绑均更换导航身份，旧写入只收尾资源，不干扰恢复后的工作区。
    ///
    /// @author kongweiguang
    pub fn open_workspace_transfer(
        &mut self,
        left: FileEndpoint,
        right: Option<FileEndpoint>,
        locked_left_host_id: Option<HostId>,
        initial_right_selection: Option<String>,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        if !endpoint_matches_locked_host(&left, locked_left_host_id.as_ref()) {
            return Err(AppError::invalid("左侧文件 endpoint 与锁定主机不匹配"));
        }

        self.initialized = true;
        self.host_picker.target_unselected = [false, right.is_none()];
        self.host_picker.targets_initialized = true;
        self.host_picker.restored_targets = true;
        self.host_picker.expanded_side = None;
        self.locked_left_host_id = None;
        if let FileEndpoint::Local { path } = &left {
            self.local_directories[0] = path.clone();
        }
        if let Some(FileEndpoint::Local { path }) = &right {
            self.local_directories[1] = path.clone();
        }
        self.host_picker.last_emitted_endpoints = Some((left.clone(), right.clone()));
        let right_endpoint = right.unwrap_or_else(|| FileEndpoint::Local {
            path: self.local_directories[1].clone(),
        });
        self.invalidate_file_action_presentation(0);
        self.invalidate_file_action_presentation(1);
        self.panels[0].endpoint = left;
        let left_endpoint = self.panels[0].endpoint.clone();
        self.panels[0].navigation_history.reset(left_endpoint);
        self.panels[0].pending_selection = None;
        self.panels[0].selected = None;
        self.panels[0].selection.clear();
        self.panels[1].endpoint = right_endpoint;
        let right_endpoint = self.panels[1].endpoint.clone();
        self.panels[1].navigation_history.reset(right_endpoint);
        self.panels[1].pending_selection = initial_right_selection.map(PendingSelection::Required);
        self.panels[1].selected = None;
        self.panels[1].selection.clear();
        self.sync_target_name_requests(cx);
        self.bookmarks.clear();
        self.bookmarks_target = None;
        self.bookmark_request_target = None;
        self.bookmark_error = None;
        self.bookmark_error_target = None;
        self.bookmark_generation = self.bookmark_generation.wrapping_add(1);
        self.pending_host_key_trust = None;
        self.status = None;
        self.delete_confirmation = None;
        self.name_action = None;
        self.active_side = 1;
        self.external_target_lease = None;
        self.load_panel(0, cx);
        if !self.host_picker.target_unselected[1] {
            self.load_panel(1, cx);
        }
        self.reload_transfers(cx);
        cx.notify();
        Ok(())
    }

    /// 原子接收 Root 转交的一次性 SFTP 目标：校验成功且 retain 后才改动当前面板。
    ///
    /// @author kongweiguang
    pub fn open_external_endpoint(
        &mut self,
        endpoint: FileEndpoint,
        lease: &ExternalTargetLease,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        let FileEndpoint::Remote { host_id, .. } = &endpoint else {
            return Err(AppError::invalid(
                "临时外部文件目标必须是 SSH/SFTP endpoint",
            ));
        };
        if host_id != lease.host_id() {
            return Err(AppError::invalid("临时连接 lease 与文件 endpoint 不匹配"));
        }

        let retained = match self.external_target_lease.as_ref() {
            Some(current) if current.host_id() == host_id => current.clone(),
            _ => lease.retain()?,
        };
        self.external_target_lease = Some(retained);
        self.open_endpoint(endpoint, cx);
        Ok(())
    }

    /// Root 在外部 workspace 确认关闭时释放 pane scope；进行中的 I/O 克隆会延长它的寿命。
    ///
    /// @author kongweiguang
    pub fn release_external_target(&mut self, host_id: &HostId) -> bool {
        if self
            .external_target_lease
            .as_ref()
            .is_some_and(|lease| lease.host_id() == host_id)
        {
            self.external_target_lease = None;
            true
        } else {
            false
        }
    }

    /// 根视图同步偏好主题；页面绘制统一消费 Kit 的当前语义色板。
    pub fn set_theme_mode(&mut self, _mode: ThemeMode, cx: &mut Context<Self>) {
        cx.notify();
    }

    /// 首次绘制才启动 I/O，并为左右列表各创建一个持久焦点 owner，避免随虚拟行反复分配。
    ///
    /// @author kongweiguang
    fn initialize(&mut self, cx: &mut Context<Self>) {
        for panel in &mut self.panels {
            if panel.list_focus_handle.is_none() {
                panel.list_focus_handle = Some(cx.focus_handle().tab_stop(true));
            }
        }
        if self.initialized {
            return;
        }
        self.initialized = true;
        self.sync_target_name_requests(cx);
        if !self.compact_layout {
            self.load_panel(0, cx);
        }
        if (!self.host_picker.enabled || !self.host_picker.target_unselected[1])
            && (!self.compact_layout
                || !matches!(&self.panels[1].endpoint, FileEndpoint::Local { .. }))
        {
            self.load_panel(1, cx);
        }
        self.reload_transfers(cx);
    }

    /// 手动刷新同时核对左右目录与统一传输队列，未选择远端时不重复读取左侧本机目录。
    ///
    /// @author kongweiguang
    pub(super) fn refresh_workbench(&mut self, cx: &mut Context<Self>) {
        for side in 0..2 {
            if !self.host_picker.enabled || !self.host_picker.target_unselected[side] {
                self.refresh_panel(side, cx);
            }
        }
        self.reload_transfers(cx);
    }

    /// 为当前远端文件用例借用 pane scope；异步操作持有 clone 到 IO 清理结束。
    ///
    /// @author kongweiguang
    fn external_target_lease_for_endpoint(
        &self,
        endpoint: &FileEndpoint,
    ) -> Option<ExternalTargetLease> {
        let lease = self.external_target_lease.as_ref()?;
        (endpoint.host_id() == Some(lease.host_id())).then(|| lease.clone())
    }

    /// 仅当传输端点实际引用当前临时 HostId 时传递 lease，保存主机仍走普通队列入口。
    ///
    /// @author kongweiguang
    fn external_target_lease_for_request(
        &self,
        request: &TransferRequest,
    ) -> Option<ExternalTargetLease> {
        let lease = self.external_target_lease.as_ref()?;
        [&request.source, &request.destination]
            .into_iter()
            .any(|endpoint| endpoint.host_id() == Some(lease.host_id()))
            .then(|| lease.clone())
    }

    /// 首次取得真实 Window 后创建输入框；纯 Enter 提交当前名称动作，组合输入和修饰 Enter 留给 Input。
    ///
    /// @author kongweiguang
    fn ensure_name_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.name_input.is_some() {
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(locale::text(
                "新文件夹名称 / 重命名",
                "Folder name / rename",
            ))
        });
        let subscription = cx.subscribe(&input, |pane, input, event, cx| match event {
            InputEvent::Change => cx.notify(),
            InputEvent::PressEnter {
                secondary: false,
                shift: false,
            } if pane.name_action.is_some()
                && pane.delete_confirmation.is_none()
                && !input.read(cx).is_composing() =>
            {
                pane.confirm_global_dialog(cx);
            }
            InputEvent::PressEnter { .. } | InputEvent::Focus | InputEvent::Blur => {}
        });
        self.name_input = Some(input);
        self._name_subscription = Some(subscription);
    }

    /// 用户或完成通知刷新时保留正在导航的请求 endpoint；没有在途导航才回退到已提交目录。
    ///
    /// @author kongweiguang
    fn refresh_panel(&mut self, side: usize, cx: &mut Context<Self>) {
        let panel = &self.panels[side];
        let Some(directory) = panel_refresh_target(
            &panel.endpoint,
            panel.requested_endpoint.as_ref(),
            panel.loading,
        )
        .cloned() else {
            return;
        };
        self.target_names.invalidate_endpoint(&directory);
        self.sync_target_name_requests(cx);
        self.permissions.clear_unsupported_in_directory(&directory);
        self.permissions.reset_selection();
        self.load_panel_at(side, directory, cx);
    }

    /// 开始创建目录并冻结目的目录；紧凑栏通过 Root 弹框提交，宽工作台保留原内联编辑。
    /// 名称和权限操作共用表单槽；原请求回执收尾前不能替换草稿或清除它的 busy 所有权。
    ///
    /// @author kongweiguang
    pub(crate) fn prepare_create_directory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.name_action_busy || self.permissions.busy {
            return;
        }
        self.ensure_name_input(window, cx);
        let side = self.active_side.min(1);
        let target = self.panels[side].endpoint.clone();
        let default_path = child_endpoint(&target, "new-folder")
            .map(|endpoint| render::browser::endpoint_path(&endpoint))
            .unwrap_or_default();
        if let Some(input) = &self.name_input {
            input.update(cx, |state, cx| {
                state.set_placeholder(locale::text("新目录路径", "New directory path"), window, cx);
                state.set_value(default_path, window, cx);
            });
        }
        self.name_action = Some(NameAction::CreateDirectory);
        self.name_action_side = Some(side);
        self.name_action_target = Some(target);
        self.name_action_busy = false;
        self.permission_dialog_target = None;
        self.delete_confirmation = None;
        self.status = None;
        cx.notify();
    }

    /// 开始重命名时冻结所选 endpoint 并预填短名称，路径的父目录仍由 endpoint 类型保留。
    /// 已提交的文件表单保持独占，防止旧回执释放后来操作的 busy。
    ///
    /// @author kongweiguang
    fn prepare_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.name_action_busy || self.permissions.busy {
            return;
        }
        self.ensure_name_input(window, cx);
        let side = self.active_side.min(1);
        let target = self.panels[side]
            .selected
            .as_ref()
            .map(|entry| entry.endpoint.clone());
        let name = target.as_ref().and_then(endpoint_name).unwrap_or_default();
        if let Some(input) = &self.name_input {
            input.update(cx, |state, cx| {
                state.set_placeholder(locale::text("新名称", "New name"), window, cx);
                state.set_value(name, window, cx);
            });
        }
        self.name_action = Some(NameAction::Rename);
        self.name_action_side = Some(side);
        self.name_action_target = target;
        self.name_action_busy = false;
        self.permission_dialog_target = None;
        self.delete_confirmation = None;
        self.status = None;
        cx.notify();
    }

    /// 验证名称为单一目录项后调用 FileService；成功由目录回读呈现，仅失败保留反馈，避免额外状态条。
    /// busy 独占该请求到真实完成；先释放操作所有权，再核验表单和目录代次，旧操作不打断新导航。
    ///
    /// @author kongweiguang
    fn commit_name_action(&mut self, cx: &mut Context<Self>) {
        if self.name_action_busy {
            return;
        }
        let Some(action) = self.name_action else {
            return;
        };
        let Some(side) = self
            .name_action_side
            .filter(|side| *side < self.panels.len())
        else {
            return;
        };
        let Some(target) = self.name_action_target.clone() else {
            self.status = Some(
                locale::text(
                    "文件操作目标已失效。",
                    "The file action target is no longer available.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let Some(input) = &self.name_input else {
            return;
        };
        let name = input.update(cx, |state, _| state.value().to_string());
        if action == NameAction::Rename && !valid_item_name(&name) {
            self.status = Some(
                locale::text(
                    "名称不能为空，且不能包含路径分隔符。",
                    "Enter a name without path separators.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let files = self.files.clone();
        let executor = self.executor.clone();
        let directory = self.panels[side].endpoint.clone();
        let generation = self.directory_navigation_generations[side];
        let external_target_lease = self.external_target_lease_for_endpoint(&target);
        match action {
            NameAction::CreateDirectory => {
                let destination = match create_directory_destination(&target, &name) {
                    Ok(destination) => destination,
                    Err(error) => {
                        self.status = Some(directory_path_error_message(error));
                        cx.notify();
                        return;
                    }
                };
                let external_target_lease = external_target_lease.clone();
                self.name_action_busy = true;
                cx.notify();
                let request_target = target.clone();
                cx.spawn(async move |this, cx| {
                    let result = executor
                        .run(move |cancel| async move {
                            let _external_target_scope = external_target_lease;
                            files.create_directory(cancel, destination).await
                        })
                        .await;
                    cx.update(|app| {
                        let _ = this.update(app, |pane, cx| {
                            pane.name_action_busy = false;
                            if pane.name_action != Some(NameAction::CreateDirectory)
                                || pane.name_action_side != Some(side)
                                || pane.name_action_target.as_ref() != Some(&request_target)
                            {
                                cx.notify();
                                return;
                            }
                            if !pane.file_action_directory_is_current(side, generation, &directory)
                            {
                                pane.name_action = None;
                                pane.name_action_side = None;
                                pane.name_action_target = None;
                                cx.notify();
                                return;
                            }
                            let succeeded = result.is_ok();
                            match result {
                                Ok(()) => {
                                    pane.name_action = None;
                                    pane.name_action_side = None;
                                    pane.name_action_target = None;
                                    pane.status = None;
                                }
                                Err(error) => pane.status = Some(localized_error(&error)),
                            }
                            if succeeded {
                                pane.load_panel(side, cx);
                            }
                            cx.notify();
                        });
                    });
                })
                .detach();
            }
            NameAction::Rename => {
                let source = target;
                let Some(parent) = parent_endpoint(&source) else {
                    self.status = Some(
                        locale::text("根路径不能重命名。", "The root path cannot be renamed.")
                            .to_string(),
                    );
                    cx.notify();
                    return;
                };
                let Some(destination) = child_endpoint(&parent, &name) else {
                    self.status = Some(
                        locale::text(
                            "目标名称不受当前文件系统支持。",
                            "The destination file system does not support that name.",
                        )
                        .to_string(),
                    );
                    cx.notify();
                    return;
                };
                let external_target_lease = external_target_lease.clone();
                self.name_action_busy = true;
                cx.notify();
                let request_target = source.clone();
                cx.spawn(async move |this, cx| {
                    let result = executor
                        .run(move |cancel| async move {
                            let _external_target_scope = external_target_lease;
                            files.rename(cancel, source, destination).await
                        })
                        .await;
                    cx.update(|app| {
                        let _ = this.update(app, |pane, cx| {
                            pane.name_action_busy = false;
                            if pane.name_action != Some(NameAction::Rename)
                                || pane.name_action_side != Some(side)
                                || pane.name_action_target.as_ref() != Some(&request_target)
                            {
                                cx.notify();
                                return;
                            }
                            if !pane.file_action_directory_is_current(side, generation, &directory)
                            {
                                pane.name_action = None;
                                pane.name_action_side = None;
                                pane.name_action_target = None;
                                cx.notify();
                                return;
                            }
                            let succeeded = result.is_ok();
                            match result {
                                Ok(()) => {
                                    pane.name_action = None;
                                    pane.name_action_side = None;
                                    pane.name_action_target = None;
                                    pane.status = None;
                                }
                                Err(error) => pane.status = Some(localized_error(&error)),
                            }
                            if succeeded {
                                pane.load_panel(side, cx);
                            }
                            cx.notify();
                        });
                    });
                })
                .detach();
            }
        }
    }

    /// 删除批次在真实回执前独占提交；确认后冻结项目与租约，目录是否可删仍由 FileService 判断。
    /// 回执只刷新原目录代次，避免旧删除打断导航或重新读取已经失效的终端目标。
    ///
    /// @author kongweiguang
    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        if self.delete_action_busy {
            return;
        }
        let side = self.active_side;
        let entries = if self.view_mode == view_mode::FilesViewMode::Tree {
            self.panels[side]
                .tree
                .selected_entries_postorder(self.show_hidden_files)
        } else {
            self.selected_entries(side)
        };
        if entries.is_empty() {
            self.status =
                Some(locale::text("请先选择要删除的项目。", "Select items to delete.").to_string());
            cx.notify();
            return;
        }
        let already_confirmed = self.delete_confirmation.as_ref().is_some_and(|confirmed| {
            confirmed.len() == entries.len()
                && confirmed
                    .iter()
                    .zip(&entries)
                    .all(|(left, right)| left.endpoint == right.endpoint)
        });
        if !already_confirmed {
            self.delete_confirmation = Some(entries.clone());
            self.status = Some(
                locale::text(
                    format!("再次确认删除所选 {} 项。", entries.len()),
                    format!(
                        "Click delete again to confirm {} selected items.",
                        entries.len()
                    ),
                )
                .to_string(),
            );
            cx.notify();
            return;
        }

        let total = entries.len();
        let directory = self.panels[side].endpoint.clone();
        let generation = self.directory_navigation_generations[side];
        let items = entries
            .into_iter()
            .map(|entry| {
                let kind = if entry.kind == FileEntryKind::Directory {
                    DeleteTargetKind::Directory
                } else {
                    DeleteTargetKind::File
                };
                let lease = self.external_target_lease_for_endpoint(&entry.endpoint);
                (entry.name, entry.endpoint, kind, lease)
            })
            .collect::<Vec<_>>();
        self.delete_confirmation = None;
        self.delete_action_busy = true;
        let files = self.files.clone();
        let executor = self.executor.clone();
        self.status = Some(
            locale::text(
                format!("正在删除所选 {total} 项…"),
                format!("Deleting {total} selected items…"),
            )
            .to_string(),
        );
        cx.spawn(async move |this, cx| {
            let result =
                executor
                    .run(move |cancel| async move {
                        Ok(delete_file_entries(files, cancel, items).await)
                    })
                    .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    pane.delete_action_busy = false;
                    if !pane.file_action_directory_is_current(side, generation, &directory) {
                        cx.notify();
                        return;
                    }
                    pane.status = match result {
                        Ok(outcome)
                            if outcome.deleted == outcome.total
                                && outcome.cancelled_remaining == 0
                                && outcome.issues.is_empty() =>
                        {
                            None
                        }
                        Ok(outcome) => {
                            let mut message = locale::text(
                                format!("已删除 {}/{} 项。", outcome.deleted, outcome.total),
                                format!("Deleted {}/{} items.", outcome.deleted, outcome.total),
                            )
                            .to_string();
                            if outcome.cancelled_remaining > 0 {
                                message.push_str(&locale::text(
                                    format!(" 后续 {} 项已取消。", outcome.cancelled_remaining),
                                    format!(
                                        " {} remaining items were cancelled.",
                                        outcome.cancelled_remaining
                                    ),
                                ));
                            }
                            if !outcome.issues.is_empty() {
                                message.push_str(&format!(
                                    " {}: {}",
                                    locale::text("失败项", "Failed items"),
                                    outcome.issues.join(&locale::text("；", "; "))
                                ));
                            }
                            Some(message)
                        }
                        Err(error) => Some(localized_error(&error)),
                    };
                    pane.load_panel(side, cx);
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// 确认仍由原选择校验和原删除用例执行；选择变化后不会删除旧快照或其它目标。
    ///
    /// @author kongweiguang
    pub(crate) fn confirm_delete_confirmation(&mut self, cx: &mut Context<Self>) {
        if self.delete_confirmation.is_some() {
            self.delete_selected(cx);
        }
    }

    /// 关闭模态只撤销确认，不发送文件操作；原列表与选择可继续使用。
    ///
    /// @author kongweiguang
    pub(crate) fn cancel_delete_confirmation(&mut self, cx: &mut Context<Self>) {
        if self.delete_confirmation.take().is_some() {
            self.status = None;
            cx.notify();
        }
    }

    /// 工具栏文件上传只打开文件选择器，业务仍由共用队列受理与去重。
    ///
    /// @author kongweiguang
    pub(super) fn prompt_upload_files(&mut self, cx: &mut Context<Self>) {
        self.prompt_upload_kind(UploadSelectionKind::Files, cx);
    }

    /// 工具栏目录上传只允许选择目录，避免系统对话框中误选文件。
    ///
    /// @author kongweiguang
    pub(super) fn prompt_upload_directories(&mut self, cx: &mut Context<Self>) {
        self.prompt_upload_kind(UploadSelectionKind::Directories, cx);
    }

    /// 所有入口共享同一提交闩锁和受监督上传流程，仅选择器范围因菜单命令而异。
    ///
    /// @author kongweiguang
    fn prompt_upload_kind(&mut self, selection_kind: UploadSelectionKind, cx: &mut Context<Self>) {
        let destination_directory = self.panels[self.active_side].endpoint.clone();
        self.prompt_upload_kind_to(destination_directory, selection_kind, cx);
    }

    /// 右键上传固定菜单捕获目录，并把选择器期间的来源检查与目标快照串成一个未提交计划。
    /// 选择器等待复用现有取消令牌，UI 不为竞速等待额外依赖运行时实现。
    ///
    /// @author kongweiguang
    fn prompt_upload_kind_to(
        &mut self,
        destination_directory: FileEndpoint,
        selection_kind: UploadSelectionKind,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.capture_transfer_target_for_directory(&destination_directory)
        else {
            self.status = Some(
                locale::text(
                    "目标目录尚未读取完成，请刷新后重试。",
                    "The destination folder is not ready. Refresh it and try again.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let mut related_sides = [false; 2];
        related_sides[target.side] = true;
        let Some((generation, cancellation)) = self.begin_transfer_preparation(related_sides, cx)
        else {
            return;
        };
        let conflict_policy = self.queue.conflict_policy;
        let external_target_lease = self.external_target_lease_for_endpoint(&destination_directory);
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: !matches!(selection_kind, UploadSelectionKind::Directories),
            directories: !matches!(selection_kind, UploadSelectionKind::Files),
            multiple: true,
            prompt: Some(locale::text(
                match selection_kind {
                    UploadSelectionKind::Files => "选择要传入当前目录的文件",
                    UploadSelectionKind::Directories => "选择要传入当前目录的文件夹",
                },
                match selection_kind {
                    UploadSelectionKind::Files => "Choose files to upload here",
                    UploadSelectionKind::Directories => "Choose folders to upload here",
                },
            )),
        });
        let files = self.files.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |cancel| async move {
                    let selected = cancel
                        .run_until_cancelled(selected)
                        .await
                        .ok_or_else(|| AppError::cancelled("上传计划已取消"))?
                        .map_err(|_| AppError::cancelled("文件选择已取消"))?
                        .map_err(|_| AppError::infrastructure("系统文件选择器无法打开"))?;
                    let Some(paths) = selected else {
                        return Ok(None);
                    };
                    let total = paths.len();
                    let mut items = Vec::with_capacity(total);
                    let mut issues = Vec::new();
                    for (index, path) in paths.into_iter().enumerate() {
                        if cancel.is_cancelled() {
                            issues.push(format!(
                                "{} {remaining} {}",
                                locale::text("已取消，", "Cancelled;"),
                                locale::text("项未处理。", "items were not processed."),
                                remaining = total - index
                            ));
                            break;
                        }
                        let Some(name) = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .map(str::to_owned)
                        else {
                            issues.push(format!(
                                "{} {} {}",
                                locale::text("第", "Item"),
                                index + 1,
                                locale::text("项的本机文件名不能安全表示。", "has a local name that cannot be represented safely.")
                            ));
                            continue;
                        };
                        let source = match files
                            .inspect_local_transfer_source(cancel.child_token(), path)
                            .await
                        {
                            Ok(entry) => entry,
                            Err(error) if cancel.is_cancelled() => {
                                issues.push(format!(
                                    "{} {remaining} {} {}",
                                    locale::text("已取消，", "Cancelled;"),
                                    locale::text("项未处理：", "items were not processed:"),
                                    localized_error(&error),
                                    remaining = total - index
                                ));
                                break;
                            }
                            Err(error) => {
                                issues.push(format!("{name}: {}", localized_error(&error)));
                                continue;
                            }
                        };
                        let kind = if source.kind == FileEntryKind::Directory {
                            TransferKind::Directory
                        } else if source.kind == FileEntryKind::File {
                            TransferKind::File
                        } else if source.kind == FileEntryKind::Symlink {
                            issues.push(locale::text(
                                format!("{name}: 上传不跟随符号链接。"),
                                format!("{name}: uploads do not follow symbolic links."),
                            ).to_string());
                            continue;
                        } else {
                            issues.push(locale::text(
                                format!("{name}: 仅支持普通文件和目录上传。"),
                                format!("{name}: only regular files and directories can be uploaded."),
                            ).to_string());
                            continue;
                        };
                        let Some(destination) = child_endpoint(&destination_directory, &name)
                        else {
                            issues.push(locale::text(
                                format!("{name}: 目标文件系统不支持该名称。"),
                                format!("{name}: the destination file system does not support this name."),
                            ).to_string());
                            continue;
                        };
                        let request = TransferRequest {
                            source: source.endpoint,
                            destination,
                            kind,
                            conflict_policy,
                            idle_timeout_seconds: None,
                        };
                        items.push(TransferEnqueueItem {
                            name,
                            request,
                            external_target_lease: external_target_lease.clone(),
                        });
                    }
                    if cancel.is_cancelled() {
                        return Err(AppError::cancelled("上传计划已取消"));
                    }
                    Ok(Some((items, total, issues)))
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    if !pane.transfer_preparation_is_current(generation) {
                        return;
                    }
                    match result {
                        Ok(Some((items, total, issues))) => pane.enqueue_transfer_batch_started(
                            TransferBatchPlan {
                                items,
                                total,
                                included_by_selected_directories: 0,
                                issues,
                                labels: BatchEnqueueLabels {
                                    action_zh: "已加入上传任务",
                                    action_en: "Upload tasks queued",
                                },
                                target,
                            },
                            cx,
                        ),
                        Ok(None) => pane.finish_cancelled_transfer(cx),
                        Err(error) => {
                            pane.finish_unsubmitted_transfer(localized_error(&error), cx);
                        }
                    }
                });
            });
        })
        .detach();
    }

    /// 菜单中的上传文件命令保留其显式目录目标，不改变当前文件栏路径。
    ///
    /// @author kongweiguang
    pub(super) fn prompt_upload_files_to(
        &mut self,
        destination_directory: FileEndpoint,
        cx: &mut Context<Self>,
    ) {
        self.prompt_upload_kind_to(destination_directory, UploadSelectionKind::Files, cx);
    }

    /// 菜单中的上传文件夹命令复用批量队列，并保留其显式目录目标。
    ///
    /// @author kongweiguang
    pub(super) fn prompt_upload_directories_to(
        &mut self,
        destination_directory: FileEndpoint,
        cx: &mut Context<Self>,
    ) {
        self.prompt_upload_kind_to(destination_directory, UploadSelectionKind::Directories, cx);
    }

    /// 本机名称按宿主平台校验，远端 POSIX 身份不能被 Windows 解释为子目录或数据流。
    /// 保留单文件保存框与批量目录选择，确认路径后才构造目标并检查覆盖风险。
    ///
    /// @author kongweiguang
    fn prompt_download(&mut self, cx: &mut Context<Self>) {
        let side = self.active_side;
        let (selected, included_by_selected_directories) =
            if self.view_mode == view_mode::FilesViewMode::Tree {
                let plan = self.panels[side]
                    .tree
                    .transfer_selection_plan(self.show_hidden_files);
                (plan.roots, plan.included_descendants)
            } else {
                (self.selected_entries(side), 0)
            };
        if selected.is_empty() {
            self.status = Some(
                locale::text(
                    "请先选择要下载的文件或目录。",
                    "Select files or folders to download.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let Some(target) = self.capture_transfer_target_snapshot(side) else {
            self.status = Some(
                locale::text(
                    "来源目录尚未读取完成，请刷新后重试。",
                    "The source folder is not ready. Refresh it and try again.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };

        let total = selected.len() + included_by_selected_directories;
        let conflict_policy = self.queue.conflict_policy;
        let mut sources = Vec::new();
        let mut issues = Vec::new();
        for entry in selected {
            let Some(name) = endpoint_name(&entry.endpoint).filter(|name| {
                FileEndpoint::Local {
                    path: PathBuf::new(),
                }
                .join_name(name)
                .is_ok()
            }) else {
                issues.push(
                    locale::text(
                        format!("{}: 文件名无法安全保存到本机，请先重命名。", entry.name),
                        format!(
                            "{}: rename this item before saving it to this device.",
                            entry.name
                        ),
                    )
                    .to_string(),
                );
                continue;
            };
            let kind = match entry.kind {
                FileEntryKind::File => TransferKind::File,
                FileEntryKind::Directory => TransferKind::Directory,
                FileEntryKind::Symlink | FileEntryKind::Other => {
                    issues.push(
                        locale::text(
                            format!("{}: 不跟随符号链接或传输特殊文件。", entry.name),
                            format!(
                                "{}: symbolic links and special files are not downloaded.",
                                entry.name
                            ),
                        )
                        .to_string(),
                    );
                    continue;
                }
            };
            if matches!(&entry.endpoint, FileEndpoint::Local { .. }) {
                issues.push(
                    locale::text(
                        format!("{}: 本机项目不需要下载。", entry.name),
                        format!("{}: this item is already local.", entry.name),
                    )
                    .to_string(),
                );
                continue;
            }
            sources.push(DownloadSourceItem {
                name,
                external_target_lease: self.external_target_lease_for_endpoint(&entry.endpoint),
                source: entry.endpoint,
                kind,
            });
        }
        if sources.is_empty() {
            self.status = batch_enqueue_message(
                "下载任务已入队",
                "Download tasks queued",
                BatchEnqueueOutcome {
                    total,
                    planned_tasks: 0,
                    queued: 0,
                    included_by_selected_directories,
                    issues,
                    cancelled_remaining: 0,
                },
            );
            cx.notify();
            return;
        }

        let mut related_sides = [false; 2];
        related_sides[side] = true;
        let Some((generation, cancellation)) = self.begin_transfer_preparation(related_sides, cx)
        else {
            return;
        };
        let target_directory = match &self.panels[0].endpoint {
            FileEndpoint::Local { path } => path.clone(),
            _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        let executor = self.executor.clone();
        if sources.len() == 1 && sources[0].kind == TransferKind::File && total == 1 {
            let Some(source) = sources.pop() else {
                self.finish_unsubmitted_transfer(
                    locale::text(
                        "所选下载项已失效，请刷新目录后重试。",
                        "The selected download item is no longer available. Refresh and try again.",
                    )
                    .to_string(),
                    cx,
                );
                return;
            };
            let selected_path = cx.prompt_for_new_path(&target_directory, Some(&source.name));
            cx.spawn(async move |this, cx| {
                let result = executor
                    .run_cancellable(cancellation, move |cancel| async move {
                        let path = cancel
                            .run_until_cancelled(selected_path)
                            .await
                            .ok_or_else(|| AppError::cancelled("下载计划已取消"))?
                            .map_err(|_| AppError::cancelled("保存位置选择已取消"))?
                            .map_err(|_| AppError::infrastructure("系统保存对话框无法打开"))?;
                        if cancel.is_cancelled() {
                            return Err(AppError::cancelled("下载计划已取消"));
                        }
                        let Some(path) = path else { return Ok(None) };
                        let item = TransferEnqueueItem {
                            name: source.name,
                            external_target_lease: source.external_target_lease,
                            request: TransferRequest {
                                source: source.source,
                                destination: FileEndpoint::Local { path },
                                kind: source.kind,
                                conflict_policy,
                                idle_timeout_seconds: None,
                            },
                        };
                        Ok(Some(vec![item]))
                    })
                    .await;
                cx.update(|app| {
                    let _ = this.update(app, |pane, cx| {
                        if !pane.transfer_preparation_is_current(generation) {
                            return;
                        }
                        match result {
                            Ok(Some(items)) => pane.enqueue_transfer_batch_started(
                                TransferBatchPlan {
                                    items,
                                    total: 1,
                                    included_by_selected_directories: 0,
                                    issues,
                                    labels: BatchEnqueueLabels {
                                        action_zh: "下载任务已入队",
                                        action_en: "Download tasks queued",
                                    },
                                    target,
                                },
                                cx,
                            ),
                            Ok(None) => pane.finish_cancelled_transfer(cx),
                            Err(error) => {
                                pane.finish_unsubmitted_transfer(localized_error(&error), cx)
                            }
                        }
                    });
                });
            })
            .detach();
            return;
        }

        let selected_path = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(locale::text(
                "选择批量下载项目的本机父目录",
                "Choose the local parent folder for the selected downloads",
            )),
        });
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |cancel| async move {
                    let paths = cancel
                        .run_until_cancelled(selected_path)
                        .await
                        .ok_or_else(|| AppError::cancelled("下载计划已取消"))?
                        .map_err(|_| AppError::cancelled("下载目录选择已取消"))?
                        .map_err(|_| AppError::infrastructure("系统目录选择器无法打开"))?;
                    let Some(mut paths) = paths else {
                        return Ok(None);
                    };
                    if cancel.is_cancelled() {
                        return Err(AppError::cancelled("下载计划已取消"));
                    }
                    let Some(parent) = paths.pop() else {
                        return Ok(None);
                    };
                    let parent_endpoint = FileEndpoint::Local { path: parent };
                    let mut items = Vec::new();
                    for source in sources {
                        let Some(destination) = child_endpoint(&parent_endpoint, &source.name)
                        else {
                            issues.push(
                                locale::text(
                                    format!("{}: 本机目标名称无效。", source.name),
                                    format!(
                                        "{}: the local destination name is invalid.",
                                        source.name
                                    ),
                                )
                                .to_string(),
                            );
                            continue;
                        };
                        items.push(TransferEnqueueItem {
                            name: source.name,
                            request: TransferRequest {
                                source: source.source,
                                destination,
                                kind: source.kind,
                                conflict_policy,
                                idle_timeout_seconds: None,
                            },
                            external_target_lease: source.external_target_lease,
                        });
                    }
                    Ok(Some((items, issues)))
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    if !pane.transfer_preparation_is_current(generation) {
                        return;
                    }
                    match result {
                        Ok(Some((items, issues))) => pane.enqueue_transfer_batch_started(
                            TransferBatchPlan {
                                items,
                                total,
                                included_by_selected_directories,
                                issues,
                                labels: BatchEnqueueLabels {
                                    action_zh: "下载任务已入队",
                                    action_en: "Download tasks queued",
                                },
                                target,
                            },
                            cx,
                        ),
                        Ok(None) => pane.finish_cancelled_transfer(cx),
                        Err(error) => pane.finish_unsubmitted_transfer(localized_error(&error), cx),
                    }
                });
            });
        })
        .detach();
    }

    /// 目录读取由受监督执行器负责，generation 防止慢响应覆盖用户刚切换的路径。
    ///
    /// @author kongweiguang
    fn load_panel(&mut self, side: usize, cx: &mut Context<Self>) {
        let endpoint = self.panels[side].endpoint.clone();
        self.load_panel_at(side, endpoint, cx);
    }

    /// 重试使用失败请求的精确端点，保留旧清单时也不能静默回到原目录。
    ///
    /// @author kongweiguang
    fn retry_panel_load(&mut self, side: usize, cx: &mut Context<Self>) {
        let endpoint = self.panels[side]
            .failed_directory
            .clone()
            .unwrap_or_else(|| self.panels[side].endpoint.clone());
        self.load_panel_at(side, endpoint, cx);
    }

    /// 新导航取消旧根 provider 并撤销原目录表单；同目录刷新保留滚动、选择和独立操作反馈。
    /// 只有原读取错误随重试清理；待选项按来源区分必需恢复和传输目标尚未发布。
    ///
    /// @author kongweiguang
    fn load_panel_at(&mut self, side: usize, endpoint: FileEndpoint, cx: &mut Context<Self>) {
        if side == 1 {
            self.terminal_directory_follow_generation = None;
        }
        let previous_intent = self.panels[side]
            .requested_endpoint
            .as_ref()
            .or(self.panels[side].failed_directory.as_ref())
            .unwrap_or(&self.panels[side].endpoint);
        let changing_directory = previous_intent != &endpoint;
        if changing_directory {
            self.invalidate_file_action_presentation(side);
        }
        self.cancel_transfer_preparation_for_side(side, cx);
        self.panels[side]
            .keyboard_focus
            .reset_for_directory(&endpoint);
        if self.panels[side].endpoint != endpoint {
            self.panels[side].list_scroll = VirtualListScrollHandle::new();
        }
        let (generation, cancellation) = {
            let panel = &mut self.panels[side];
            panel.directory_error = None;
            panel.failed_directory = None;
            let generation = begin_panel_load_request(
                &mut panel.generation,
                &mut panel.loading,
                &mut panel.requested_endpoint,
                &endpoint,
            );
            let cancellation = panel.begin_root_request_cancellation(generation);
            (generation, cancellation)
        };
        let requested_endpoint = endpoint.clone();
        self.panels[side]
            .tree
            .begin_root_request(&endpoint, generation);
        let external_target_lease = self.external_target_lease_for_endpoint(&endpoint);
        let files = self.files.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |cancel| async move {
                    let _external_target_scope = external_target_lease;
                    files.list_directory(cancel, endpoint).await
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    let locked_left_host_id = pane.locked_left_host_id.clone();
                    let panel = &mut pane.panels[side];
                    panel.finish_root_request_cancellation(generation);
                    if !finish_panel_load_request(
                        panel.generation,
                        generation,
                        &mut panel.requested_endpoint,
                        &requested_endpoint,
                    ) {
                        return;
                    }
                    if side == 1 && pane.terminal_directory_follow_generation == Some(generation) {
                        pane.terminal_directory_follow_generation = None;
                    }
                    panel.loading = false;
                    panel.directory_error = None;
                    panel.failed_directory = None;
                    match result {
                        Ok(listing) => {
                            if side == 0
                                && !endpoint_matches_locked_host(
                                    &listing.directory,
                                    locked_left_host_id.as_ref(),
                                )
                            {
                                pane.panels[side].listing = None;
                                pane.status = Some(
                                    locale::text(
                                        "文件服务返回了锁定主机以外的路径。",
                                        "The file service returned a path outside the locked host.",
                                    )
                                    .to_string(),
                                );
                                cx.notify();
                                return;
                            }
                            let show_hidden_files = pane.show_hidden_files;
                            pane.panels[side].tree.set_root_listing(
                                listing.directory.clone(),
                                generation,
                                listing.entries.clone(),
                                show_hidden_files,
                            );
                            let mut visible_entries = listing
                                .entries
                                .iter()
                                .filter(|entry| {
                                    show_hidden_files || !render::browser::is_hidden_entry(entry)
                                })
                                .collect::<Vec<_>>();
                            visible_entries.sort_by(|left, right| {
                                render::browser::entry_sort_key(left)
                                    .cmp(&render::browser::entry_sort_key(right))
                                    .then_with(|| {
                                        left.name.to_lowercase().cmp(&right.name.to_lowercase())
                                    })
                            });
                            let visible_names = visible_entries
                                .iter()
                                .map(|entry| entry.name.clone())
                                .collect::<Vec<_>>();
                            let keyboard_visible_names = visible_entries
                                .iter()
                                .filter(|entry| pane.panels[side].entry_filter.matches(entry.kind))
                                .map(|entry| entry.name.clone())
                                .collect::<Vec<_>>();
                            let panel = &mut pane.panels[side];
                            if panel.endpoint != listing.directory {
                                panel.selection.clear();
                                panel.selected = None;
                            }
                            panel.endpoint = listing.directory.clone();
                            let requested_selection = panel.pending_selection.take();
                            let selected = if let Some(selection) = requested_selection.as_ref() {
                                let name = match selection {
                                    PendingSelection::Required(name)
                                    | PendingSelection::IfPresent(name) => name,
                                };
                                listing
                                    .entries
                                    .iter()
                                    .find(|entry| &entry.name == name)
                                    .cloned()
                            } else {
                                panel.selected.as_ref().and_then(|previous| {
                                    listing
                                        .entries
                                        .iter()
                                        .find(|entry| entry.endpoint == previous.endpoint)
                                        .cloned()
                                })
                            };
                            if requested_selection.is_some() {
                                panel.selection.clear();
                                if let Some(entry) = selected.as_ref() {
                                    panel.selection.select_only(&entry.name);
                                }
                            }
                            panel.selection.retain_visible(&visible_names);
                            panel
                                .keyboard_focus
                                .retain_visible(&listing.directory, &keyboard_visible_names);
                            if matches!(requested_selection, Some(PendingSelection::Required(_)))
                                && selected.is_none()
                            {
                                pane.status = Some(
                                    locale::text(
                                        "恢复的选择项当前不可用。",
                                        "The restored selection is no longer available.",
                                    )
                                    .to_string(),
                                );
                            }
                            panel.selected = selected;
                            panel.listing = Some(listing);
                            if let FileEndpoint::Local { path } = &panel.endpoint {
                                pane.local_directories[side] = path.clone();
                            }
                            pane.complete_active_host_tab_refresh(side);
                            if side == pane.active_side {
                                pane.load_bookmarks(side, cx);
                            }
                        }
                        Err(error) => {
                            let error_message = localized_error(&error);
                            panel.directory_error = Some(error_message);
                            panel.failed_directory = Some(requested_endpoint.clone());
                            pane.panels[side].pending_selection = None;
                        }
                    }
                    pane.sync_target_name_requests(cx);
                    pane.emit_transfer_endpoints_changed(cx);
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 同步双面板目标集合；同一 HostId 或容器身份只查询一次。
    ///
    /// @author kongweiguang
    fn sync_target_name_requests(&mut self, cx: &mut Context<Self>) {
        let endpoints = self
            .panels
            .iter()
            .map(|panel| panel.endpoint.clone())
            .collect::<Vec<_>>();
        let requests = self
            .target_names
            .synchronize(endpoints, &self.target_name_lookup);
        for request in requests {
            let lookup = self.target_name_lookup.clone();
            let executor = self.executor.clone();
            let key = request.key;
            let query_key = key.clone();
            let generation = request.generation;
            cx.spawn(async move |this, cx| {
                let result = executor
                    .run_cancellable(
                        request.cancellation,
                        move |operation_cancellation| async move {
                            lookup.resolve(query_key, operation_cancellation).await
                        },
                    )
                    .await;
                cx.update(|app| {
                    let _ = this.update(app, |pane, cx| {
                        if pane.target_names.complete(&key, generation, result) {
                            cx.notify();
                        }
                    });
                });
            })
            .detach();
        }
    }

    /// 当前活动 endpoint 的书签按目标身份缓存；代次和目标校验共同拒绝迟到的旧响应。
    ///
    /// @author kongweiguang
    fn load_bookmarks(&mut self, side: usize, cx: &mut Context<Self>) {
        if side != self.active_side {
            return;
        }
        let endpoint = self.panels[side].endpoint.clone();
        let Some((target, _)) = bookmark_target(&endpoint) else {
            self.bookmark_generation = self.bookmark_generation.wrapping_add(1);
            self.bookmark_request_target = None;
            self.bookmark_error = None;
            self.bookmark_error_target = None;
            return;
        };
        if self.bookmarks_target.as_ref() == Some(&target) {
            if self.bookmark_request_target.is_some() {
                self.bookmark_generation = self.bookmark_generation.wrapping_add(1);
                self.bookmark_request_target = None;
            }
            self.bookmark_error = None;
            self.bookmark_error_target = None;
            return;
        }
        if self.bookmark_request_target.as_ref() == Some(&target) {
            return;
        }
        self.bookmark_generation = self.bookmark_generation.wrapping_add(1);
        let generation = self.bookmark_generation;
        self.bookmark_request_target = Some(target.clone());
        self.bookmark_error = None;
        self.bookmark_error_target = None;
        let external_target_lease = self.external_target_lease_for_endpoint(&endpoint);
        let files = self.files.clone();
        let executor = self.executor.clone();
        let requested_target = target.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    let _external_target_scope = external_target_lease;
                    files.list_bookmarks(cancel, requested_target).await
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    if pane.bookmark_generation != generation
                        || pane.bookmark_request_target.as_ref() != Some(&target)
                    {
                        return;
                    }
                    pane.bookmark_request_target = None;
                    let current_endpoint = &pane.panels[pane.active_side].endpoint;
                    if !bookmark_response_is_current(
                        generation,
                        pane.bookmark_generation,
                        &target,
                        current_endpoint,
                    ) {
                        return;
                    }
                    match result {
                        Ok(bookmarks) => {
                            pane.bookmarks = bookmarks;
                            pane.bookmarks_target = Some(target);
                            pane.bookmark_error = None;
                            pane.bookmark_error_target = None;
                        }
                        Err(error) => {
                            let message = localized_error(&error);
                            pane.bookmark_error = Some(message.clone());
                            pane.bookmark_error_target = Some(target.clone());
                            pane.status = Some(message);
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// 目录项进入目录，普通文件保留显式编辑入口。
    /// 先撤销旧表单的展示资格，再提交目录，避免迟到写入刷新覆盖新位置。
    ///
    /// @author kongweiguang
    fn activate_entry(&mut self, side: usize, entry: FileEntry, cx: &mut Context<Self>) {
        self.delete_confirmation = None;
        if side == 0
            && !endpoint_matches_locked_host(&entry.endpoint, self.locked_left_host_id.as_ref())
        {
            self.status = Some(
                locale::text(
                    "文件项不属于左侧锁定的主机。",
                    "The file entry is outside the host locked to the left panel.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let switched_side = self.active_side != side;
        self.active_side = side;
        if entry.kind == FileEntryKind::Directory {
            self.invalidate_file_action_presentation(side);
            self.panels[side]
                .navigation_history
                .push(entry.endpoint.clone());
            self.panels[side].endpoint = entry.endpoint;
            self.panels[side].pending_selection = None;
            self.panels[side].selected = None;
            self.panels[side].selection.clear();
            self.load_panel(side, cx);
            return;
        }
        if switched_side {
            self.load_bookmarks(side, cx);
        }
        if let Some(name) = endpoint_name(&entry.endpoint) {
            self.panels[side].selection.select_only(&name);
        }
        self.panels[side].selected = Some(entry);
        cx.notify();
    }

    /// 返回一层目录的父路径；POSIX 远端路径不经过 Windows Path 规则解释。
    /// 父目录导航与地址栏跳转共用写入回执失效约束，不因预写 endpoint 而漏掉导航身份。
    ///
    /// @author kongweiguang
    fn navigate_parent(&mut self, side: usize, cx: &mut Context<Self>) {
        let endpoint = match &self.panels[side].endpoint {
            FileEndpoint::Local { path } => path.parent().map(|parent| FileEndpoint::Local {
                path: parent.to_path_buf(),
            }),
            FileEndpoint::Remote { host_id, path } => {
                path.parent().map(|path| FileEndpoint::Remote {
                    host_id: host_id.clone(),
                    path,
                })
            }
            FileEndpoint::Container {
                host_id,
                runtime,
                container_id,
                path,
            } => path.parent().map(|path| FileEndpoint::Container {
                host_id: host_id.clone(),
                runtime: *runtime,
                container_id: container_id.clone(),
                path,
            }),
        };
        let Some(endpoint) = endpoint else { return };
        if self.host_picker.target_pending[side]
            && let Some(side) = FilePaneSide::from_index(side)
        {
            self.cancel_pending_host_selection(side, cx);
        }
        if side == 0 && !endpoint_matches_locked_host(&endpoint, self.locked_left_host_id.as_ref())
        {
            self.status = Some(
                locale::text(
                    "父目录不属于左侧锁定的主机。",
                    "The parent directory is outside the host locked to the left panel.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        self.panels[side].navigation_history.push(endpoint.clone());
        self.invalidate_file_action_presentation(side);
        self.panels[side].endpoint = endpoint;
        self.panels[side].pending_selection = None;
        self.panels[side].selected = None;
        self.panels[side].selection.clear();
        self.load_panel(side, cx);
        self.emit_transfer_endpoints_changed(cx);
    }

    /// 将指定面板的当前路径加入或移除目标级书签；回执按面板端点校验，切换活动侧不丢结果。
    ///
    /// @author kongweiguang
    fn toggle_bookmark(&mut self, side: usize, cx: &mut Context<Self>) {
        let endpoint = self.panels[side].endpoint.clone();
        let Some((target, path)) = bookmark_target(&endpoint) else {
            self.status = Some(
                locale::text(
                    "当前路径无法安全表示为书签。",
                    "This path cannot be safely bookmarked.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let bookmarked = self
            .bookmarks
            .iter()
            .any(|bookmark| bookmark.target == target && bookmark.path == path);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .unwrap_or_default();
        let bookmark = FileBookmark {
            created_at_unix_ms: timestamp,
            target: target.clone(),
            path,
            label: None,
        };
        self.write_bookmark(side, bookmark, !bookmarked, cx);
    }

    /// 移除菜单中捕获的书签前再次校验目标身份，迟到菜单不能改动另一主机的收藏。
    ///
    /// @author kongweiguang
    pub(super) fn remove_bookmark(
        &mut self,
        side: usize,
        bookmark: FileBookmark,
        cx: &mut Context<Self>,
    ) {
        self.write_bookmark(side, bookmark, false, cx);
    }

    /// 书签写入按 target 和 generation 接受回执，减少重复实现并拒绝旧主机返回值。
    ///
    /// @author kongweiguang
    fn write_bookmark(
        &mut self,
        side: usize,
        bookmark: FileBookmark,
        bookmarked: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(endpoint) = self.panels.get(side).map(|panel| panel.endpoint.clone()) else {
            return;
        };
        let Some((target, _)) = bookmark_target(&endpoint) else {
            return;
        };
        if bookmark.target != target {
            self.status = Some(
                locale::text(
                    "书签属于另一个文件目标，未作修改。",
                    "The bookmark belongs to another file target and was not changed.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let external_target_lease = self.external_target_lease_for_endpoint(&endpoint);
        self.bookmark_generation = self.bookmark_generation.wrapping_add(1);
        let generation = self.bookmark_generation;
        self.bookmark_request_target = None;
        self.bookmark_error = None;
        self.bookmark_error_target = None;
        if self.bookmarks_target.as_ref() == Some(&target) {
            self.bookmarks_target = None;
        }
        let files = self.files.clone();
        let executor = self.executor.clone();
        let request_target = target.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    let _external_target_scope = external_target_lease;
                    files.set_bookmark(cancel, bookmark, bookmarked).await
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    let current_endpoint = &pane.panels[side].endpoint;
                    if !bookmark_response_is_current(
                        generation,
                        pane.bookmark_generation,
                        &request_target,
                        current_endpoint,
                    ) {
                        return;
                    }
                    match result {
                        Ok(bookmarks) => {
                            pane.bookmarks = bookmarks;
                            pane.bookmarks_target = Some(request_target);
                            pane.bookmark_error = None;
                            pane.bookmark_error_target = None;
                        }
                        Err(error) => {
                            let message = localized_error(&error);
                            pane.bookmark_error = Some(message.clone());
                            pane.bookmark_error_target = Some(request_target.clone());
                            pane.status = Some(message);
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// 将当前选择批量复制到另一面板；每个任务冻结冲突策略并保留自身临时连接租约。
    ///
    /// @author kongweiguang
    fn transfer_selected(&mut self, cx: &mut Context<Self>) {
        if self.transfer_submission_busy {
            return;
        }
        let source_side = self.active_side;
        let destination_side = 1 - source_side;
        if !self.can_transfer_selected_to_other_side(source_side) {
            self.status = Some(
                locale::text(
                    "请选择两侧主机并等待目录加载完成。",
                    "Choose both hosts and wait for their directories to load.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let Some(target) = self.capture_transfer_target_snapshot(destination_side) else {
            self.status = Some(
                locale::text(
                    "目标目录尚未读取完成，请刷新后重试。",
                    "The destination folder is not ready. Refresh it and try again.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let source_directory = self.panels[source_side].endpoint.clone();
        let destination_directory = self.panels[destination_side].endpoint.clone();
        let selected = self.selected_entries(source_side);
        let (selected, included_by_selected_directories) =
            if self.view_mode == view_mode::FilesViewMode::Tree {
                drag_drop::copy_selection_roots(selected, &source_directory, &destination_directory)
            } else {
                (selected, 0)
            };
        if selected.is_empty() {
            self.status = Some(
                locale::text("请先选择文件或目录。", "Select files or folders first.").to_string(),
            );
            cx.notify();
            return;
        }

        let total = selected.len() + included_by_selected_directories;
        let (items, issues) = drag_drop::plan_copy_entries(
            &selected,
            &source_directory,
            &destination_directory,
            self.queue.conflict_policy,
        );
        let items = items
            .into_iter()
            .map(|(name, request)| TransferEnqueueItem {
                name,
                external_target_lease: self.external_target_lease_for_request(&request),
                request,
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            self.status = batch_enqueue_message(
                "复制任务已入队",
                "Copy tasks queued",
                BatchEnqueueOutcome {
                    total,
                    included_by_selected_directories,
                    issues,
                    ..BatchEnqueueOutcome::default()
                },
            );
            cx.notify();
            return;
        }
        self.enqueue_transfer_batch(
            TransferBatchPlan {
                items,
                total,
                included_by_selected_directories,
                issues,
                labels: BatchEnqueueLabels {
                    action_zh: "复制任务已入队",
                    action_en: "Copy tasks queued",
                },
                target,
            },
            cx,
        );
    }

    /// 开始一次冻结批次的冲突预检；只有读取成功且目标快照仍有效时才打开决策或提交队列。
    ///
    /// @author kongweiguang
    fn enqueue_transfer_batch(&mut self, plan: TransferBatchPlan, cx: &mut Context<Self>) {
        if !self.begin_transfer_submission(cx) {
            return;
        }
        self.enqueue_transfer_batch_started(plan, cx);
    }

    /// 接收已持有提交闩锁的异步计划；冲突索引与冻结计划一同回传，避免界面改用后来的目标。
    ///
    /// @author kongweiguang
    fn enqueue_transfer_batch_started(&mut self, plan: TransferBatchPlan, cx: &mut Context<Self>) {
        let Some((generation, cancellation)) =
            self.transfer_submission.as_ref().and_then(|submission| {
                (submission.phase == TransferSubmissionPhase::Preparing)
                    .then(|| (submission.generation, submission.cancellation.clone()))
            })
        else {
            return;
        };
        if !self.transfer_target_is_current(&plan.target) {
            self.finish_unsubmitted_transfer(
                locale::text(
                    "目标目录已变化，传输任务未提交。",
                    "The destination changed. No transfer tasks were submitted.",
                )
                .to_string(),
                cx,
            );
            return;
        }

        let files = self.files.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |cancellation| async move {
                    let conflicts =
                        find_transfer_conflicts(&files, &cancellation, &plan.items).await?;
                    Ok((plan, conflicts))
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    if !pane.transfer_preparation_is_current(generation) {
                        return;
                    }
                    match result {
                        Ok((plan, conflicts)) if pane.transfer_target_is_current(&plan.target) => {
                            if conflicts.is_empty() {
                                pane.submit_transfer_batch(plan, cx);
                            } else {
                                pane.status = None;
                                pane.pending_transfer_conflict =
                                    Some(PendingTransferConflict { plan, conflicts });
                                cx.notify();
                            }
                        }
                        Ok(_) => pane.finish_unsubmitted_transfer(
                            locale::text(
                                "目标目录已变化，传输任务未提交。",
                                "The destination changed. No transfer tasks were submitted.",
                            )
                            .to_string(),
                            cx,
                        ),
                        Err(error) => pane.finish_unsubmitted_transfer(localized_error(&error), cx),
                    }
                });
            });
        })
        .detach();
    }

    /// 冻结请求经冲突预检后一次性提交；此处是目标验证后的提交边界。
    ///
    /// @author kongweiguang
    fn submit_transfer_batch(&mut self, plan: TransferBatchPlan, cx: &mut Context<Self>) {
        if !self.transfer_target_is_current(&plan.target) {
            self.finish_unsubmitted_transfer(
                locale::text(
                    "目标目录已变化，传输任务未提交。",
                    "The destination changed. No transfer tasks were submitted.",
                )
                .to_string(),
                cx,
            );
            return;
        }
        let Some(submission) = self.transfer_submission.as_mut() else {
            return;
        };
        if submission.phase != TransferSubmissionPhase::Preparing {
            return;
        }
        submission.phase = TransferSubmissionPhase::Enqueuing;
        let generation = submission.generation;
        let origin = submission.origin.clone();
        let result_origin = origin.clone();
        let transfers = self.transfers.clone();
        let executor = self.executor.clone();
        let labels = plan.labels;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    Ok(enqueue_transfer_items(
                        transfers,
                        origin,
                        cancel,
                        plan.items,
                        plan.total,
                        plan.included_by_selected_directories,
                        plan.issues,
                    )
                    .await)
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    if !pane.transfer_submission.as_ref().is_some_and(|submission| {
                        submission.generation == generation
                            && submission.phase == TransferSubmissionPhase::Enqueuing
                    }) {
                        return;
                    }
                    pane.end_transfer_submission();
                    pane.pending_transfer_conflict = None;
                    if pane.transfer_origin.as_ref() != Some(&result_origin) {
                        pane.reload_transfers(cx);
                        return;
                    }
                    match result {
                        Ok(outcome) => {
                            pane.status =
                                batch_enqueue_message(labels.action_zh, labels.action_en, outcome);
                            pane.reload_transfers(cx);
                        }
                        Err(error) => pane.status = Some(localized_error(&error)),
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// 选择即提交 main 定义的冲突策略；目标过期时关闭弹框并丢弃尚未入队的冻结批次。
    ///
    /// @author kongweiguang
    fn resolve_transfer_conflict(&mut self, policy: ConflictPolicy, cx: &mut Context<Self>) {
        let Some(mut pending) = self.pending_transfer_conflict.take() else {
            return;
        };
        if !self.transfer_target_is_current(&pending.plan.target) {
            self.finish_unsubmitted_transfer(
                locale::text(
                    "目标目录已变化，传输任务未提交。",
                    "The destination changed. No transfer tasks were submitted.",
                )
                .to_string(),
                cx,
            );
            return;
        }
        for item in &mut pending.plan.items {
            item.request.conflict_policy = policy;
        }
        self.submit_transfer_batch(pending.plan, cx);
    }

    /// 暂停在确认前的批次只在目标端点及 generation 原样保留时有效。
    ///
    /// @author kongweiguang
    fn capture_transfer_target_snapshot(&self, side: usize) -> Option<TransferTargetSnapshot> {
        let panel = self.panels.get(side)?;
        if self.host_picker.enabled
            && (self.host_picker.target_unselected[side] || self.host_picker.target_pending[side])
        {
            return None;
        }
        (!panel.loading
            && panel.requested_endpoint.is_none()
            && panel
                .listing
                .as_ref()
                .is_some_and(|listing| listing.directory == panel.endpoint))
        .then(|| TransferTargetSnapshot {
            side,
            endpoint: panel.endpoint.clone(),
            generation: panel.generation,
        })
    }

    /// 菜单捕获行前只检查两侧目录可用性；实际选择在动作提交时另行校验。
    ///
    /// @author kongweiguang
    pub(super) fn can_transfer_selected_to_other_side(&self, side: usize) -> bool {
        can_transfer_to_other_side(
            self.host_picker.enabled,
            self.host_picker.target_unselected,
            side,
        ) && !self.transfer_submission_busy
            && self.capture_transfer_target_snapshot(side).is_some()
            && self.capture_transfer_target_snapshot(1 - side).is_some()
    }

    /// 相同目录同时出现在两侧时优先原活动侧；只接受当前目录或仍可见的真实目录行。
    ///
    /// @author kongweiguang
    fn capture_transfer_target_for_directory(
        &self,
        directory: &FileEndpoint,
    ) -> Option<TransferTargetSnapshot> {
        for side in [self.active_side, 1 - self.active_side] {
            let panel = &self.panels[side];
            let is_directory_row = panel.listing.as_ref().is_some_and(|listing| {
                listing.entries.iter().any(|entry| {
                    entry.endpoint == *directory
                        && entry.kind == FileEntryKind::Directory
                        && (self.show_hidden_files || !render::browser::is_hidden_entry(entry))
                })
            }) || (self.view_mode == view_mode::FilesViewMode::Tree
                && panel
                    .tree
                    .visible_endpoints(self.show_hidden_files)
                    .contains(directory)
                && panel
                    .tree
                    .entry(directory)
                    .is_some_and(|entry| entry.kind == FileEntryKind::Directory));
            if panel.endpoint == *directory || is_directory_row {
                return self.capture_transfer_target_snapshot(side);
            }
        }
        None
    }

    /// 只用浏览目标快照判断是否仍可提交，不把原请求改投到当前活动面板。
    ///
    /// @author kongweiguang
    fn transfer_target_is_current(&self, target: &TransferTargetSnapshot) -> bool {
        self.capture_transfer_target_snapshot(target.side)
            .is_some_and(|current| {
                current.endpoint == target.endpoint && current.generation == target.generation
            })
    }

    /// 错误、取消和导航变化都释放唯一提交闩锁，保证后续批次可明确重试。
    ///
    /// @author kongweiguang
    fn finish_unsubmitted_transfer(&mut self, message: String, cx: &mut Context<Self>) {
        self.pending_transfer_conflict = None;
        self.end_transfer_submission();
        self.status = Some(message);
        cx.notify();
    }

    /// 用户主动取消选择器只释放批次，不插入提示条；已有独立错误仍可查看或关闭。
    ///
    /// @author kongweiguang
    fn finish_cancelled_transfer(&mut self, cx: &mut Context<Self>) {
        self.pending_transfer_conflict = None;
        self.end_transfer_submission();
        cx.notify();
    }

    /// 只有相关侧的未提交准备会随目录或主机变化取消；已开始受理的批次必须等回执。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_transfer_preparation_for_side(
        &mut self,
        side: usize,
        cx: &mut Context<Self>,
    ) {
        let affected = self.transfer_submission.as_ref().is_some_and(|submission| {
            submission.phase == TransferSubmissionPhase::Preparing
                && submission.related_sides.get(side).copied().unwrap_or(false)
        });
        if affected {
            self.finish_unsubmitted_transfer(
                locale::text(
                    "文件目标或选择已变化，未提交的传输已取消。",
                    "A file target or selection changed. The unsubmitted transfer was canceled.",
                )
                .to_string(),
                cx,
            );
        }
    }

    /// 离开视图只撤销尚未受理的意图，已经入队的项目与其真实回执继续保留。
    ///
    /// @author kongweiguang
    pub(crate) fn cancel_transfer_preparation(&mut self, cx: &mut Context<Self>) {
        if self
            .transfer_submission
            .as_ref()
            .is_some_and(|submission| submission.phase == TransferSubmissionPhase::Preparing)
        {
            self.finish_cancelled_transfer(cx);
        }
    }

    /// 关闭面板前必须等待正在逐项受理的结果，避免已受理与未受理项目混淆。
    ///
    /// @author kongweiguang
    pub(crate) fn transfer_enqueue_in_progress(&self) -> bool {
        self.transfer_submission
            .as_ref()
            .is_some_and(|submission| submission.phase == TransferSubmissionPhase::Enqueuing)
    }

    /// 面板切到其它来源后仍须保留原批次的受理屏障，队列登记前不能按当前展示来源放行关闭。
    ///
    /// @author kongweiguang
    pub(crate) fn transfer_enqueue_in_progress_for_origin(&self, origin: &TransferOrigin) -> bool {
        self.transfer_submission.as_ref().is_some_and(|submission| {
            submission.phase == TransferSubmissionPhase::Enqueuing && &submission.origin == origin
        })
    }

    /// 关闭冲突弹框只丢弃未提交计划；已排队的任务不受关闭动作影响。
    ///
    /// @author kongweiguang
    fn cancel_transfer_conflict(&mut self, cx: &mut Context<Self>) {
        if self.pending_transfer_conflict.take().is_some() {
            self.finish_cancelled_transfer(cx);
        }
    }

    /// 冲突数量从冻结的逐项结果推导，避免数量与弹框对象各自漂移。
    ///
    /// @author kongweiguang
    fn pending_transfer_conflict_count(&self) -> Option<usize> {
        self.pending_transfer_conflict
            .as_ref()
            .map(|pending| pending.conflicts.len())
    }

    /// 同一文件面板只允许一个传输批次；原操作按钮表达忙态，重复提交不再插入状态条或覆盖已有错误。
    ///
    /// @author kongweiguang
    fn begin_transfer_submission(&mut self, cx: &mut Context<Self>) -> bool {
        self.begin_transfer_preparation([true, true], cx).is_some()
    }

    /// 保存实际关联侧并分配单调代次；旧选择器或预检回执不能释放后续提交。
    ///
    /// @author kongweiguang
    fn begin_transfer_preparation(
        &mut self,
        related_sides: [bool; 2],
        cx: &mut Context<Self>,
    ) -> Option<(u64, CancellationToken)> {
        if self.transfer_submission_busy {
            return None;
        }
        let origin = self.transfer_origin.clone()?;
        self.transfer_submission_generation = self.transfer_submission_generation.wrapping_add(1);
        let generation = self.transfer_submission_generation;
        let cancellation = CancellationToken::new();
        self.transfer_submission = Some(TransferSubmission {
            generation,
            origin,
            cancellation: cancellation.clone(),
            related_sides,
            phase: TransferSubmissionPhase::Preparing,
        });
        self.transfer_submission_busy = true;
        cx.notify();
        Some((generation, cancellation))
    }

    /// 只接受仍由同一未提交批次持有的回执，不让迟到错误覆盖当前状态。
    ///
    /// @author kongweiguang
    fn transfer_preparation_is_current(&self, generation: u64) -> bool {
        self.transfer_submission.as_ref().is_some_and(|submission| {
            submission.generation == generation
                && submission.phase == TransferSubmissionPhase::Preparing
                && !submission.cancellation.is_cancelled()
        })
    }

    /// 对话框取消、队列受理或执行失败都要释放面板闩锁，允许用户明确重试。
    ///
    /// @author kongweiguang
    fn end_transfer_submission(&mut self) {
        if let Some(submission) = self.transfer_submission.take() {
            submission.cancellation.cancel();
        }
        self.transfer_submission_busy = false;
    }

    /// 权威快照先于目录回读；删除补偿与同批新终态合并，避免重复读取及漏掉迟到提交。
    /// 读取仍经过 panel generation 门禁，旧路径或迟到回执不能覆盖新目录。
    ///
    /// @author kongweiguang
    fn apply_transfer_rows(
        &mut self,
        rows: Vec<TransferSummary>,
        visible_rows: Vec<TransferSummary>,
        cx: &mut Context<Self>,
    ) {
        let refresh_directories = if self.queue.take_uncertain_transfer_change() {
            vec![TransferRefreshScope::All]
        } else {
            completed_transfer_refresh_directories(&self.all_transfer_rows, &rows)
        };
        self.all_transfer_rows = rows;
        self.transfer_rows = visible_rows;
        self.invalidate_inactive_transfer_directories(&refresh_directories);
        for side in 0..self.panels.len() {
            let should_refresh = {
                let panel = &self.panels[side];
                panel_needs_transfer_refresh(
                    &panel.endpoint,
                    panel.requested_endpoint.as_ref(),
                    panel.loading,
                    panel.listing.is_some(),
                    &refresh_directories,
                )
            };
            if should_refresh {
                self.refresh_panel(side, cx);
            }
        }
    }

    /// 活队列与恢复材料共用读取代次，只接纳当前来源的最新快照。
    /// 进度广播不扫描没有界面入口的冷档，避免无用的后台读取。
    ///
    /// @author kongweiguang
    fn reload_transfers(&mut self, cx: &mut Context<Self>) {
        let generation = self.queue.begin_transfer_refresh();
        cx.notify();
        let transfers = self.transfers.clone();
        let executor = self.executor.clone();
        let origin = self.transfer_origin.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    let rows = transfers.list().await;
                    let (visible, recovery) = match origin {
                        Some(origin) => (
                            transfers.list_for_origin(&origin).await,
                            transfers
                                .list_recovery_for_origin(&origin, cancellation)
                                .await,
                        ),
                        None => (Vec::new(), Ok(Vec::new())),
                    };
                    Ok((rows, visible, recovery))
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    if !pane.queue.accepts_transfer_refresh(generation) {
                        return;
                    }
                    match result {
                        Ok((rows, visible, recovery)) => {
                            pane.apply_transfer_rows(rows, visible, cx);
                            match recovery {
                                Ok(recovery) => {
                                    pane.transfer_recovery_rows = recovery;
                                    pane.queue.finish_recovery_refresh(generation, Ok(()));
                                }
                                Err(error) => {
                                    pane.queue.finish_recovery_refresh(generation, Err(error));
                                }
                            }
                        }
                        Err(error) => {
                            if pane.queue.take_uncertain_transfer_change() {
                                pane.refresh_after_uncertain_transfer_change(cx);
                            }
                            pane.queue.finish_recovery_refresh(generation, Err(error));
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// 队列信号允许合并；活任务状态快照用于识别一次性的提交成功并刷新精确目标目录。
    /// 删除事件立即使缓存失效，并跨代次保留补偿；接受权威快照后才合并目录回读。
    /// 事件读取与按钮共用代次，同时保持串行读取以免进度广播堆积磁盘任务。
    /// 后台读取开始后立即通知，让 loading 不依赖其它窗口输入事件。
    ///
    /// @author kongweiguang
    fn watch_transfer_queue(&mut self, cx: &mut Context<Self>) {
        if self.queue_watch_started {
            return;
        }
        self.queue_watch_started = true;
        let mut events = self.transfers.subscribe();
        let executor = self.executor.clone();
        let transfers = self.transfers.clone();
        cx.spawn(async move |this, cx| {
            loop {
                let mut waiting_events = events;
                let wake = executor
                    .run(move |cancel| async move {
                        let wake = waiting_events.recv_event(cancel).await;
                        Ok((wake, waiting_events))
                    })
                    .await;
                let (wake, next_events) = match wake {
                    Ok(result) => result,
                    Err(_) => break,
                };
                events = next_events;
                let Some(wake) = wake else { break };
                let generation = cx.update(|app| {
                    this.update(app, |pane, cx| {
                        if matches!(
                            wake,
                            TransferQueueEvent::Cleared | TransferQueueEvent::Resync
                        ) {
                            pane.queue.mark_uncertain_transfer_change();
                            pane.invalidate_inactive_transfer_directories(&[
                                TransferRefreshScope::All,
                            ]);
                        }
                        let generation = pane.queue.begin_transfer_refresh();
                        cx.notify();
                        (generation, pane.transfer_origin.clone())
                    })
                });
                let Ok((generation, origin)) = generation else {
                    break;
                };
                let service = transfers.clone();
                let result = executor
                    .run(move |cancellation| async move {
                        let rows = service.list().await;
                        let (visible, recovery) = match origin {
                            Some(origin) => (
                                service.list_for_origin(&origin).await,
                                service
                                    .list_recovery_for_origin(&origin, cancellation)
                                    .await,
                            ),
                            None => (Vec::new(), Ok(Vec::new())),
                        };
                        Ok((rows, visible, recovery))
                    })
                    .await;
                cx.update(|app| {
                    let _ = this.update(app, |pane, cx| {
                        if !pane.queue.accepts_transfer_refresh(generation) {
                            return;
                        }
                        match result {
                            Ok((rows, visible, recovery)) => {
                                pane.apply_transfer_rows(rows, visible, cx);
                                match recovery {
                                    Ok(recovery) => {
                                        pane.transfer_recovery_rows = recovery;
                                        pane.queue.finish_recovery_refresh(generation, Ok(()));
                                    }
                                    Err(error) => {
                                        pane.queue.finish_recovery_refresh(generation, Err(error));
                                    }
                                }
                            }
                            Err(error) => {
                                if pane.queue.take_uncertain_transfer_change() {
                                    pane.refresh_after_uncertain_transfer_change(cx);
                                }
                                pane.queue.finish_recovery_refresh(generation, Err(error));
                            }
                        }
                        cx.notify();
                    });
                });
            }
        })
        .detach();
    }

    /// 固定当前来源后再取消，目标项回执屏障仍由全局队列负责。
    ///
    /// @author kongweiguang
    fn cancel_transfer(
        &mut self,
        id: kerminal_domain::identity::TransferId,
        cx: &mut Context<Self>,
    ) {
        let Some(origin) = self.transfer_origin.clone() else {
            return;
        };
        let result_origin = origin.clone();
        let transfers = self.transfers.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    transfers.ensure_origin(&id, &origin, cancellation).await?;
                    transfers.cancel(&id).await
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    if pane.transfer_origin.as_ref() != Some(&result_origin) {
                        return;
                    }
                    if let Err(error) = result {
                        pane.status = Some(localized_error(&error));
                    }
                    pane.reload_transfers(cx);
                });
            });
        })
        .detach();
    }

    /// 续传保留原任务，普通重试创建后继；待清理 owner 必须先确认释放才可继续。
    /// 同一忙态覆盖清理与重试，取消或来源切换不得把原结果投影到新页面。
    ///
    /// @author kongweiguang
    fn retry_transfer(
        &mut self,
        id: kerminal_domain::identity::TransferId,
        resume: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(origin) = self.transfer_origin.clone() else {
            return;
        };
        if !self.queue.begin_transfer_retry(id.clone()) {
            return;
        }
        let retry_id = id.clone();
        let result_origin = origin.clone();
        let external_target_lease = self.external_target_lease.clone();
        let transfers = self.transfers.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    transfers
                        .ensure_origin(&id, &origin, cancellation.clone())
                        .await?;
                    let summary = transfers.get(&id).await?;
                    if summary.cleanup_pending
                        && summary.failure_kind
                            != Some(kerminal_domain::files::TransferFailureKind::CommitUnknown)
                    {
                        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                        cancellation
                            .run_until_cancelled(transfers.retry_cleanup_until(&id, deadline.into()))
                            .await
                            .ok_or_else(|| AppError::cancelled("传输重试已取消，请回读清理状态"))??;
                    }
                    match temporary_host_for_request(&summary.request)? {
                        Some(required_host) => {
                            let Some(lease) = external_target_lease
                                .as_ref()
                                .filter(|lease| lease.host_id() == &required_host)
                            else {
                                return Ok(None);
                            };
                            if resume {
                                transfers.resume_with_external_target(&id, lease).await.map(Some)
                            } else {
                                transfers.retry_with_external_target(&id, lease).await.map(Some)
                            }
                        }
                        None if resume => transfers.resume(&id).await.map(Some),
                        None => transfers.retry(&id).await.map(Some),
                    }
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    pane.queue.finish_transfer_retry(&retry_id);
                    if pane.transfer_origin.as_ref() != Some(&result_origin) {
                        cx.notify();
                        return;
                    }
                    match result {
                        Ok(Some(_)) => pane.status = None,
                        Ok(None) => {
                            pane.status = Some(locale::text(
                                "原临时目标连接已失效；请重新连接同一目标后再试。此任务不会切换到新目标。",
                                "The temporary target lease expired. Reconnect to the same target and try again; this task will not switch targets."
                            ).to_string())
                        }
                        Err(error) => pane.status = Some(localized_error(&error)),
                    }
                    pane.reload_transfers(cx);
                });
            });
        })
        .detach();
        cx.notify();
    }
}

impl BrowserPanel {
    /// 新根读取替换旧 owner，并将同一 token 交给 application executor 驱动 provider 取消。
    ///
    /// @author kongweiguang
    fn begin_root_request_cancellation(&mut self, generation: u64) -> CancellationToken {
        self.cancel_root_request_cancellation();
        let cancellation = CancellationToken::new();
        self.root_request_cancellation = Some(RootDirectoryRequestCancellation {
            generation,
            cancellation: cancellation.clone(),
        });
        cancellation
    }

    /// 目标失效、tab 离开或 panel Drop 时同步通知根 provider，不改变 tree 的子树取消状态。
    ///
    /// @author kongweiguang
    fn cancel_root_request_cancellation(&mut self) {
        if let Some(request) = self.root_request_cancellation.take() {
            request.cancellation.cancel();
        }
    }

    /// 仅清理匹配的根读取 owner，迟到旧回执不能移除当前请求 token。
    ///
    /// @author kongweiguang
    fn finish_root_request_cancellation(&mut self, generation: u64) {
        if self
            .root_request_cancellation
            .as_ref()
            .is_some_and(|request| request.generation == generation)
        {
            self.root_request_cancellation = None;
        }
    }

    /// 新面板区分已提交清单与空请求，并从无键盘滚动意图开始，避免跨目录继承旧视口目标。
    ///
    /// @author kongweiguang
    fn new(endpoint: FileEndpoint) -> Self {
        let navigation_history = NavigationHistory::new(endpoint.clone());
        Self {
            endpoint,
            root_request_cancellation: None,
            requested_endpoint: None,
            listing: None,
            selected: None,
            selection: selection::FileSelection::default(),
            list_focus_handle: None,
            keyboard_focus: FileListKeyboardFocus::default(),
            pending_selection: None,
            path_input: None,
            path_input_endpoint: None,
            _path_subscription: None,
            loading: false,
            directory_error: None,
            failed_directory: None,
            generation: 0,
            tree: tree::FileTreeBrowser::default(),
            tree_widget: None,
            list_scroll: VirtualListScrollHandle::new(),
            sort: FileSort::default(),
            entry_filter: view_mode::FileEntryFilter::default(),
            navigation_history,
        }
    }
}

impl Drop for BrowserPanel {
    /// panel 释放时取消根目录与子树读取，避免关闭的视图继续拥有远程工作。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        self.cancel_root_request_cancellation();
        self.tree.cancel_pending_requests();
    }
}

impl EventEmitter<OpenFileRequested> for FilesPane {}
impl EventEmitter<FilesPaneEvent> for FilesPane {}
impl EventEmitter<ActivateEditorFileRequested> for FilesPane {}
impl EventEmitter<SftpHostRequested> for FilesPane {}
impl EventEmitter<SftpHostKeyTrustRequested> for FilesPane {}
impl EventEmitter<CreateSftpHostRequested> for FilesPane {}

/// 按稳定错误类别选择英文安全文案，中文仍使用 application 统一的脱敏消息。
///
/// @author kongweiguang
fn localized_error(error: &AppError) -> String {
    // 传输层以稳定英文上下文表达 TCP 建连失败；文件栏给出与连接面板一致的可操作说明。
    if error.public_message() == "SSH TCP connection failed" {
        return locale::text(
            "无法连接 SSH 主机，请检查网络、地址和端口后重试。",
            "Cannot connect to the SSH host. Check the network, address and port, then retry.",
        )
        .to_string();
    }
    let english = match error.public_code() {
        PublicErrorCode::Invalid => "The file request is invalid.",
        PublicErrorCode::NotFound => "The selected file or directory was not found.",
        PublicErrorCode::Unsupported => {
            "This operation is not supported for the selected location."
        }
        PublicErrorCode::CapabilityNotSupported => {
            "The connection does not support this operation."
        }
        PublicErrorCode::Conflict => "The file changed or conflicts with the current state.",
        PublicErrorCode::Cancelled => "The operation was cancelled.",
        PublicErrorCode::Infrastructure => "The file operation failed.",
        PublicErrorCode::OutcomeUnknown => {
            "The result is unknown. Check the destination before retrying."
        }
        PublicErrorCode::AuthenticationFailed => "Connection authentication failed.",
        PublicErrorCode::HostKeyUnknown => "The host key has not been trusted.",
        PublicErrorCode::HostKeyChanged => "The host key changed; the connection was blocked.",
        PublicErrorCode::HostKeyTrustMismatch => "Host key verification failed.",
        PublicErrorCode::Timeout => "The operation timed out.",
        PublicErrorCode::RemoteExit => "The remote command exited.",
        PublicErrorCode::ConfigurationCorrupt => {
            "The bookmark configuration is corrupt and needs recovery."
        }
    };
    locale::text(error.public_message(), english).to_string()
}

/// 临时文件标签取得独立 owner；没有匹配来源时拒绝打开，普通目标不需要租约。
///
/// @author kongweiguang
fn retain_file_target_lease(
    endpoint: &FileEndpoint,
    lease: Option<&ExternalTargetLease>,
) -> AppResult<Option<ExternalTargetLease>> {
    let Some(host_id) = endpoint
        .host_id()
        .filter(|host_id| host_id.as_str().starts_with("external:"))
    else {
        return Ok(None);
    };
    let lease = lease
        .filter(|lease| lease.host_id() == host_id)
        .ok_or_else(|| AppError::conflict("临时连接已失效，无法打开文件。"))?;
    lease.retain().map(Some)
}

/// 目标文件系统拥有名称规则，统一领域入口防止 UI 复制把远端名称变成本机路径。
///
/// @author kongweiguang
fn child_endpoint(endpoint: &FileEndpoint, name: &str) -> Option<FileEndpoint> {
    endpoint.join_name(name).ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectoryPathError {
    Empty,
    NotAbsolute,
    Root,
    Invalid,
}

/// 按 main 的路径输入语义解析新建目录目标；相对输入锚定当前目录，根路径保持禁止。
///
/// @author kongweiguang
fn create_directory_destination(
    current: &FileEndpoint,
    input: &str,
) -> Result<FileEndpoint, DirectoryPathError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(DirectoryPathError::Empty);
    }
    match current {
        FileEndpoint::Local { path } => {
            let entered = PathBuf::from(input);
            let destination = if entered.is_absolute() {
                entered
            } else {
                path.join(entered)
            };
            if destination.file_name().is_none() {
                return Err(DirectoryPathError::Root);
            }
            Ok(FileEndpoint::Local { path: destination })
        }
        FileEndpoint::Remote { host_id, path } => {
            let resolved = resolve_remote_directory_input(path, input);
            let destination =
                RemotePath::parse(&resolved).map_err(|_| DirectoryPathError::Invalid)?;
            if !destination.as_str().starts_with('/') {
                return Err(DirectoryPathError::NotAbsolute);
            }
            if destination.as_str() == "/" {
                return Err(DirectoryPathError::Root);
            }
            Ok(FileEndpoint::Remote {
                host_id: host_id.clone(),
                path: destination,
            })
        }
        FileEndpoint::Container {
            host_id,
            runtime,
            container_id,
            path,
        } => {
            let resolved = resolve_remote_directory_input(path, input);
            let destination =
                RemotePath::parse_container(&resolved).map_err(|_| DirectoryPathError::Invalid)?;
            if destination.as_str() == "/" {
                return Err(DirectoryPathError::Root);
            }
            Ok(FileEndpoint::Container {
                host_id: host_id.clone(),
                runtime: *runtime,
                container_id: container_id.clone(),
                path: destination,
            })
        }
    }
}

/// 使用 POSIX 分隔符处理 SFTP 输入，避免本机 Windows 路径规则改写远端路径。
///
/// @author kongweiguang
fn resolve_remote_directory_input(current: &RemotePath, input: &str) -> String {
    let input = input.replace('\\', "/");
    if input.starts_with('/') {
        input
    } else if current.as_str() == "/" {
        format!("/{input}")
    } else {
        format!("{}/{input}", current.as_str())
    }
}

/// 将路径前置校验映射为短而可纠正的表单消息，不向用户暴露解析器内部错误。
///
/// @author kongweiguang
fn directory_path_error_message(error: DirectoryPathError) -> String {
    match error {
        DirectoryPathError::Empty => {
            locale::text("请输入目录路径。", "Enter a directory path.").to_string()
        }
        DirectoryPathError::NotAbsolute => locale::text(
            "目录路径必须是绝对路径。",
            "The directory path must be absolute.",
        )
        .to_string(),
        DirectoryPathError::Root => locale::text(
            "不能在根目录路径上新建目录。",
            "A new directory path cannot be the root.",
        )
        .to_string(),
        DirectoryPathError::Invalid => {
            locale::text("目录路径无效。", "The directory path is invalid.").to_string()
        }
    }
}

/// 从 endpoint 获取末级文件名；POSIX 远端路径按斜杠解析，不经过宿主路径规则。
fn endpoint_name(endpoint: &FileEndpoint) -> Option<String> {
    match endpoint {
        FileEndpoint::Local { path } => path.file_name()?.to_str().map(str::to_owned),
        FileEndpoint::Remote { path, .. } | FileEndpoint::Container { path, .. } => path
            .as_str()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
    }
}

/// 浏览与文件导航共用父目录推导，完整保留主机、运行时和容器身份，避免跨文件系统降级。
///
/// @author kongweiguang
pub(crate) fn parent_endpoint(endpoint: &FileEndpoint) -> Option<FileEndpoint> {
    match endpoint {
        FileEndpoint::Local { path } => Some(FileEndpoint::Local {
            path: path.parent()?.to_path_buf(),
        }),
        FileEndpoint::Remote { host_id, path } => Some(FileEndpoint::Remote {
            host_id: host_id.clone(),
            path: path.parent()?,
        }),
        FileEndpoint::Container {
            host_id,
            runtime,
            container_id,
            path,
        } => Some(FileEndpoint::Container {
            host_id: host_id.clone(),
            runtime: *runtime,
            container_id: container_id.clone(),
            path: path.parent()?,
        }),
    }
}

/// 加载中以用户最新请求路径为刷新目标；成功响应提交后则使用已确认的浏览目录。
///
/// @author kongweiguang
fn panel_refresh_target<'a>(
    committed_endpoint: &'a FileEndpoint,
    requested_endpoint: Option<&'a FileEndpoint>,
    loading: bool,
) -> Option<&'a FileEndpoint> {
    if loading {
        requested_endpoint
    } else {
        Some(committed_endpoint)
    }
}

/// 加载开始时同步冻结请求 endpoint 与 generation，保证测试和异步列表回执遵守同一请求语义。
///
/// @author kongweiguang
fn begin_panel_load_request(
    generation: &mut u64,
    loading: &mut bool,
    requested_endpoint: &mut Option<FileEndpoint>,
    endpoint: &FileEndpoint,
) -> u64 {
    *generation = (*generation).wrapping_add(1);
    *loading = true;
    *requested_endpoint = Some(endpoint.clone());
    *generation
}

/// 只有当前代次与其请求 endpoint 同时吻合时才允许回执结束 pending，迟到失败/取消不清新请求。
///
/// @author kongweiguang
fn finish_panel_load_request(
    current_generation: u64,
    request_generation: u64,
    pending_endpoint: &mut Option<FileEndpoint>,
    response_endpoint: &FileEndpoint,
) -> bool {
    if current_generation != request_generation
        || pending_endpoint.as_ref() != Some(response_endpoint)
    {
        return false;
    }
    *pending_endpoint = None;
    true
}

#[cfg(test)]
#[path = "../../../tests/unit/files_transfer_completion_refresh.rs"]
mod transfer_completion_refresh_tests;
/// 判断两个 endpoint 是否处于同一逻辑文件系统，用于队列定位时复用正确一侧浏览器。
///
/// @author kongweiguang
///
/// @author kongweiguang
fn endpoint_same_filesystem(left: &FileEndpoint, right: &FileEndpoint) -> bool {
    match (left, right) {
        (FileEndpoint::Local { .. }, FileEndpoint::Local { .. }) => true,
        (
            FileEndpoint::Remote { host_id: left, .. },
            FileEndpoint::Remote { host_id: right, .. },
        ) => left == right,
        (
            FileEndpoint::Container {
                host_id: left_host,
                runtime: left_runtime,
                container_id: left_id,
                ..
            },
            FileEndpoint::Container {
                host_id: right_host,
                runtime: right_runtime,
                container_id: right_id,
                ..
            },
        ) => left_host == right_host && left_runtime == right_runtime && left_id == right_id,
        _ => false,
    }
}

/// 这里只排除跨目录和不可显示的名称；反斜杠在 POSIX 上合法，目标平台限制由 child_endpoint 核对。
///
/// @author kongweiguang
fn valid_item_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.chars().any(char::is_control)
}

/// 左侧工作区若声明主机锁，只允许 endpoint 保持在该连接身份下。
///
/// @author kongweiguang
fn endpoint_matches_locked_host(endpoint: &FileEndpoint, locked_host_id: Option<&HostId>) -> bool {
    locked_host_id.is_none_or(|locked| endpoint.host_id() == Some(locked))
}

/// 从持久任务请求识别临时目标身份；重试不能根据当前面板猜测原任务连接。
///
/// @author kongweiguang
fn temporary_host_for_request(request: &TransferRequest) -> AppResult<Option<HostId>> {
    let mut temporary_host = None;
    for endpoint in [&request.source, &request.destination] {
        let Some(host_id) = endpoint
            .host_id()
            .filter(|host_id| host_id.as_str().starts_with("external:"))
        else {
            continue;
        };
        if temporary_host
            .as_ref()
            .is_some_and(|current: &HostId| current != host_id)
        {
            return Err(AppError::invalid(
                "传输任务引用了多个临时连接，拒绝猜测重试目标",
            ));
        }
        temporary_host = Some(host_id.clone());
    }
    Ok(temporary_host)
}

/// 仅在 bookmark target 与当前目录的协议身份一致时构造导航 endpoint。
fn bookmark_endpoint(current: &FileEndpoint, bookmark: &FileBookmark) -> Option<FileEndpoint> {
    match (current, &bookmark.target) {
        (FileEndpoint::Local { .. }, BookmarkTarget::Local) => Some(FileEndpoint::Local {
            path: PathBuf::from(&bookmark.path),
        }),
        (FileEndpoint::Remote { .. }, BookmarkTarget::Remote { host_id }) => {
            Some(FileEndpoint::Remote {
                host_id: host_id.clone(),
                path: kerminal_domain::files::RemotePath::parse(&bookmark.path).ok()?,
            })
        }
        (
            FileEndpoint::Container { .. },
            BookmarkTarget::Container {
                host_id,
                runtime,
                container_id,
            },
        ) => Some(FileEndpoint::Container {
            host_id: host_id.clone(),
            runtime: *runtime,
            container_id: container_id.clone(),
            path: kerminal_domain::files::RemotePath::parse(&bookmark.path).ok()?,
        }),
        _ => None,
    }
}

/// 将本机、SSH 和容器路径映射到独立书签作用域，路径为当前 directory 本身。
fn bookmark_target(endpoint: &FileEndpoint) -> Option<(BookmarkTarget, String)> {
    match endpoint {
        FileEndpoint::Local { path } => Some((BookmarkTarget::Local, path.to_str()?.to_owned())),
        FileEndpoint::Remote { host_id, path } => Some((
            BookmarkTarget::Remote {
                host_id: host_id.clone(),
            },
            path.as_str().to_owned(),
        )),
        FileEndpoint::Container {
            host_id,
            runtime,
            container_id,
            path,
        } => Some((
            BookmarkTarget::Container {
                host_id: host_id.clone(),
                runtime: *runtime,
                container_id: container_id.clone(),
            },
            path.as_str().to_owned(),
        )),
    }
}

/// 书签响应只有在请求代次仍有效且活动 endpoint 属于同一目标时才允许更新界面。
///
/// @author kongweiguang
fn bookmark_response_is_current(
    request_generation: u64,
    current_generation: u64,
    requested_target: &BookmarkTarget,
    current_endpoint: &FileEndpoint,
) -> bool {
    request_generation == current_generation
        && bookmark_target(current_endpoint)
            .is_some_and(|(current_target, _)| &current_target == requested_target)
}

#[cfg(test)]
#[path = "../../../tests/unit/files_initial_remote_binding.rs"]
mod initial_remote_binding_tests;

#[cfg(test)]
#[path = "../../../tests/unit/files_transfer_conflicts.rs"]
mod transfer_conflict_tests;

#[cfg(test)]
#[path = "../../../tests/unit/files_action_support.rs"]
pub(crate) mod action_test_support;

#[cfg(test)]
#[path = "../../../tests/unit/files_action_regressions.rs"]
mod action_regression_tests;

#[cfg(test)]
#[path = "../../../tests/unit/files_transfer_interactions.rs"]
mod transfer_interaction_tests;

#[cfg(test)]
#[path = "../../../tests/unit/files_dialog_interactions.rs"]
mod dialog_interaction_tests;

#[cfg(test)]
#[path = "../../../tests/unit/files_root_request_cancellation.rs"]
mod root_request_cancellation_tests;
