//! 本机与远端文件工作区；页面只编排文件、书签与传输用例。
//!
//! @author kongweiguang

mod drag_drop;
mod host_picker;
mod permissions;
mod render;
mod selection;
mod target_names;
mod transfer_queue;
mod tree;
mod view_mode;

pub use host_picker::{CreateSftpHostRequested, SftpHostRequested};
pub(crate) use target_names::{FileTargetNameLookup, FileTargetNameState, SftpHostChoice};

use std::{
    cell::Cell,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex},
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
    files::{FileService, MAX_FILE_PREVIEW_BYTES},
    platform::external_launch::ExternalTargetLease,
    preferences::ThemeMode,
    transfers::{TransferRecoveryRecord, TransferService},
};
use kerminal_domain::files::{
    BookmarkTarget, DeleteTargetKind, DirectoryListing, FileBookmark, FileEndpoint, FileEntry,
    FileEntryKind, FilePreview, TransferKind, TransferRequest, TransferStatus, TransferSummary,
};
use kerminal_domain::identity::HostId;
use tokio_util::sync::CancellationToken;

use crate::{features::editor::EditorWorkspaceProjection, locale};

/// 文件面板请求根视图打开独立编辑器 tab，编辑器脏状态由根窗口统一管理。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenFileRequested {
    pub endpoint: FileEndpoint,
}

/// 文件工作区请求 Root 提供当前编辑器 tab 的只读快照，不共享编辑器实体。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceOverviewRequested {
    pub(crate) generation: u64,
}

/// 工作区中的文件行请求 Root 激活仍然存在的 editor endpoint。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActivateEditorFileRequested {
    pub(crate) endpoint: FileEndpoint,
}

/// 左右面板各自维护当前目录和选择，切换目标不会重建后台传输任务。
struct BrowserPanel {
    endpoint: FileEndpoint,
    /// 正在读取时保留用户最新请求的目录；成功回执前 endpoint 仍是旧的已提交清单位置。
    requested_endpoint: Option<FileEndpoint>,
    listing: Option<DirectoryListing>,
    selected: Option<FileEntry>,
    selection: selection::FileSelection,
    /// 每侧只持有一个列表焦点句柄，虚拟行滚动复用同一复合控件焦点。
    list_focus_handle: Option<gpui_kit::FocusHandle>,
    keyboard_focus: FileListKeyboardFocus,
    pending_selection: Option<String>,
    pending_file_preview: bool,
    path_input: Option<Entity<InputState>>,
    path_input_endpoint: Option<String>,
    _path_subscription: Option<Subscription>,
    loading: bool,
    generation: u64,
    preview_generation: u64,
    preview_cancellation: Option<Arc<PreviewTaskCancellation>>,
    tree: tree::FileTreeBrowser,
    tree_widget: Option<tree::TreeWidget>,
    list_scroll: VirtualListScrollHandle,
    keyboard_scroll_intent: KeyboardScrollIntent,
    sort: FileSort,
    entry_filter: view_mode::FileEntryFilter,
    navigation_history: NavigationHistory,
}

/// 键盘活动项按目录与文件名绑定；虚拟列表滚动和排序不会留下行索引焦点。
///
/// @author kongweiguang
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct FileListKeyboardFocus {
    directory: Option<FileEndpoint>,
    active_name: Option<String>,
}

/// 平面文件行共用此高度，让翻页按实际滚动视口换算，不依赖窗口尺寸或固定页数。
///
/// @author kongweiguang
pub(super) const FILE_LIST_ROW_HEIGHT_PX: f32 = 44.0;

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

/// 键盘移动后的补滚动只在目标、目录与预览代次仍匹配时生效。
///
/// @author kongweiguang
#[derive(Clone)]
struct PendingKeyboardScroll {
    revision: u64,
    panel_generation: u64,
    directory: FileEndpoint,
    target_name: String,
    target_endpoint: FileEndpoint,
    preview_generation: u64,
    wait_for_preview: bool,
    consumed: Rc<Cell<bool>>,
}

/// 保存单个尚未消费的键盘可见性请求，鼠标滚动或新输入会使旧 revision 失效。
///
/// @author kongweiguang
#[derive(Clone, Default)]
struct KeyboardScrollIntent {
    revision: u64,
    pending: Option<PendingKeyboardScroll>,
}

/// 当前预览/列表快照是否仍允许消费一次延迟键盘滚动。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyboardScrollReadiness {
    Stale,
    WaitingForPreview,
    Ready,
}

/// 一次 prepaint 用的只读状态快照，避免重复读取并保证评估与消费使用相同上下文。
///
/// @author kongweiguang
#[derive(Clone)]
struct KeyboardScrollSnapshot {
    panel_generation: u64,
    directory: FileEndpoint,
    listing_is_current: bool,
    active_name: Option<String>,
    selected_endpoint: Option<FileEndpoint>,
    preview_generation: u64,
    preview_pending: bool,
}

impl KeyboardScrollIntent {
    /// 用新键盘目标替换旧意图，预览文件仅等待本次请求结束后再校正 viewport。
    ///
    /// @author kongweiguang
    fn arm(
        &mut self,
        panel_generation: u64,
        directory: &FileEndpoint,
        target_name: &str,
        target_endpoint: &FileEndpoint,
        preview_generation: u64,
        wait_for_preview: bool,
    ) -> u64 {
        if let Some(previous) = self.pending.take() {
            previous.consumed.set(true);
        }
        self.revision = self.revision.wrapping_add(1);
        self.pending = Some(PendingKeyboardScroll {
            revision: self.revision,
            panel_generation,
            directory: directory.clone(),
            target_name: target_name.to_owned(),
            target_endpoint: target_endpoint.clone(),
            preview_generation,
            wait_for_preview,
            consumed: Rc::new(Cell::new(false)),
        });
        self.revision
    }

    /// 取消等待中的补滚动并标记共享令牌，让已捕获的 Canvas 闭包也看见失效。
    ///
    /// @author kongweiguang
    fn cancel(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.consumed.set(true);
        }
        self.revision = self.revision.wrapping_add(1);
    }

    /// 只撤销创建该 Canvas 时仍然存活的意图，不误伤更新后的键盘目标。
    ///
    /// @author kongweiguang
    fn cancel_if_revision(&mut self, revision: u64) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.revision == revision)
        {
            self.cancel();
        }
    }

    /// 对照活动文件与已提交清单，预览仍在读取时等待；其它代次或目标变化视为过期。
    ///
    /// @author kongweiguang
    fn readiness_for(
        &self,
        revision: u64,
        snapshot: &KeyboardScrollSnapshot,
    ) -> KeyboardScrollReadiness {
        let Some(pending) = self
            .pending
            .as_ref()
            .filter(|pending| pending.revision == revision)
        else {
            return KeyboardScrollReadiness::Stale;
        };
        if !snapshot.listing_is_current
            || pending.panel_generation != snapshot.panel_generation
            || pending.directory != snapshot.directory
            || snapshot.active_name.as_deref() != Some(pending.target_name.as_str())
            || snapshot.selected_endpoint.as_ref() != Some(&pending.target_endpoint)
            || pending.preview_generation != snapshot.preview_generation
            || pending.consumed.get()
        {
            return KeyboardScrollReadiness::Stale;
        }
        if pending.wait_for_preview && snapshot.preview_pending {
            KeyboardScrollReadiness::WaitingForPreview
        } else {
            KeyboardScrollReadiness::Ready
        }
    }

    /// 单次消费 ready 意图；失效快照清掉旧项，仍在读取的预览保留到下一次布局。
    ///
    /// @author kongweiguang
    fn consume_if_ready(&mut self, revision: u64, snapshot: &KeyboardScrollSnapshot) -> bool {
        match self.readiness_for(revision, snapshot) {
            KeyboardScrollReadiness::Ready => {
                let Some(consumed) = self
                    .pending
                    .as_ref()
                    .filter(|pending| pending.revision == revision)
                    .map(|pending| Rc::clone(&pending.consumed))
                else {
                    return false;
                };
                if consumed.replace(true) {
                    return false;
                }
                self.cancel();
                true
            }
            KeyboardScrollReadiness::WaitingForPreview => false,
            KeyboardScrollReadiness::Stale => {
                self.cancel_if_revision(revision);
                false
            }
        }
    }
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

/// 每个浏览侧独立保留有界路径历史；回退/前进是视图导航，不创建或重绑会话。
///
/// @author kongweiguang
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct NavigationHistory {
    entries: Vec<FileEndpoint>,
    cursor: usize,
}

impl NavigationHistory {
    /// 初始化历史时将构造端点作为唯一当前项，避免首次后退跳到空目标。
    ///
    /// @author kongweiguang
    fn new(endpoint: FileEndpoint) -> Self {
        Self {
            entries: vec![endpoint],
            cursor: 0,
        }
    }

    /// 切换连接身份时丢弃旧主机路径，避免回退重新暴露已失效的 SFTP 目标。
    ///
    /// @author kongweiguang
    fn reset(&mut self, endpoint: FileEndpoint) {
        self.entries.clear();
        self.entries.push(endpoint);
        self.cursor = 0;
    }

    /// 用户主动改变目录后截断前进分支，并限制历史长度以免长期浏览累积无界路径。
    ///
    /// @author kongweiguang
    fn push(&mut self, endpoint: FileEndpoint) {
        if self.entries.get(self.cursor) == Some(&endpoint) {
            return;
        }
        self.entries.truncate(self.cursor.saturating_add(1));
        self.entries.push(endpoint);
        if self.entries.len() > MAX_NAVIGATION_HISTORY {
            let overflow = self.entries.len() - MAX_NAVIGATION_HISTORY;
            self.entries.drain(..overflow);
        }
        self.cursor = self.entries.len().saturating_sub(1);
    }

    /// 回退只移动视图历史游标，调用方随后按原 endpoint 重读目录。
    ///
    /// @author kongweiguang
    fn back(&mut self) -> Option<FileEndpoint> {
        if self.cursor == 0 || self.entries.is_empty() {
            return None;
        }
        self.cursor -= 1;
        self.entries.get(self.cursor).cloned()
    }

    /// 前进只移动视图历史游标，调用方随后按原 endpoint 重读目录。
    ///
    /// @author kongweiguang
    fn forward(&mut self) -> Option<FileEndpoint> {
        if self.cursor.saturating_add(1) >= self.entries.len() {
            return None;
        }
        self.cursor += 1;
        self.entries.get(self.cursor).cloned()
    }

    /// 工具栏只在存在前一项时启用回退操作。
    ///
    /// @author kongweiguang
    fn can_go_back(&self) -> bool {
        self.cursor > 0
    }

    /// 工具栏只在存在后一项时启用前进操作。
    ///
    /// @author kongweiguang
    fn can_go_forward(&self) -> bool {
        self.cursor.saturating_add(1) < self.entries.len()
    }
}

const MAX_NAVIGATION_HISTORY: usize = 64;

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
    left_navigation_count: usize,
    left_host_locked: bool,
) -> bool {
    host_picker_enabled
        && right_target_unselected
        && matches!(left_endpoint, FileEndpoint::Local { .. })
        && left_navigation_count == 1
        && !left_host_locked
}

/// 共享工作台右侧尚未显式绑定主机时，拒绝左侧向隐藏本机占位目标发起复制。
/// 独立文件面板和远端复制回本机不受此选择器状态限制。
///
/// @author kongweiguang
fn can_transfer_to_other_side(
    host_picker_enabled: bool,
    right_target_unselected: bool,
    source_side: usize,
) -> bool {
    !(host_picker_enabled && right_target_unselected && source_side == 0)
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

/// 绑定受监督任务 token，使切换选择或关闭 pane 能协作取消当前预览 IO。
#[derive(Default)]
struct PreviewTaskCancellation {
    state: Mutex<PreviewTaskCancellationState>,
}

#[derive(Default)]
struct PreviewTaskCancellationState {
    cancelled: bool,
    task_token: Option<CancellationToken>,
}

impl PreviewTaskCancellation {
    /// 将 executor 的任务 token 交给 pane 侧取消句柄；绑定前已失效的请求会立即取消。
    ///
    /// @author kongweiguang
    fn bind_task_token(&self, token: CancellationToken) {
        let cancel_now = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.cancelled {
                true
            } else {
                state.task_token = Some(token.clone());
                false
            }
        };
        if cancel_now {
            token.cancel();
        }
    }

    /// 清除已经完成的任务 token，避免后续 pane 状态继续持有已结束的取消句柄。
    ///
    /// @author kongweiguang
    fn clear_task_token(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.task_token = None;
    }

    /// 先记录请求失效再取消任务，覆盖取消早于 executor 启动的竞态。
    ///
    /// @author kongweiguang
    fn cancel(&self) {
        let task_token = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.cancelled = true;
            state.task_token.take()
        };
        if let Some(token) = task_token {
            token.cancel();
        }
    }
}

/// GPUI 文件工作区维持左右浏览、预览、书签与统一传输队列的轻量视图状态。
pub struct FilesPane {
    files: FileService,
    transfers: TransferService,
    executor: ApplicationTaskExecutor,
    target_name_lookup: FileTargetNameLookup,
    target_names: FileTargetNameState,
    panels: [BrowserPanel; 2],
    locked_left_host_id: Option<HostId>,
    external_target_lease: Option<ExternalTargetLease>,
    active_side: usize,
    follow_terminal_directory: bool,
    current_terminal_endpoint: Option<FileEndpoint>,
    preview: Option<FilePreview>,
    bookmarks: Vec<FileBookmark>,
    bookmarks_target: Option<BookmarkTarget>,
    bookmark_request_target: Option<BookmarkTarget>,
    bookmark_generation: u64,
    transfer_rows: Vec<TransferSummary>,
    transfer_recovery_rows: Vec<TransferRecoveryRecord>,
    queue: transfer_queue::TransferQueueUi,
    host_picker: host_picker::HostPickerState,
    transfer_submission_busy: bool,
    permissions: permissions::PermissionEditor,
    compact_layout: bool,
    view_mode: view_mode::FilesViewMode,
    browser_mode: view_mode::FilesViewMode,
    workspace_projection: Option<EditorWorkspaceProjection>,
    workspace_generation: u64,
    /// 已应用的编辑器快照版本跨视图保留，拒绝隐藏期间迟到的旧推送。
    workspace_projection_revision: Option<u64>,
    workspace_scroll: VirtualListScrollHandle,
    show_hidden_files: bool,
    status: Option<String>,
    status_details_expanded: bool,
    initialized: bool,
    queue_watch_started: bool,
    delete_confirmation: Option<Vec<FileEndpoint>>,
    name_input: Option<Entity<InputState>>,
    _name_subscription: Option<Subscription>,
    name_action: Option<NameAction>,
}

/// 共用短名称输入仅表达当前待执行的单项文件动作，不缓存或拼接不可信路径。
#[derive(Clone, Copy, PartialEq, Eq)]
enum NameAction {
    CreateDirectory,
    Rename,
}

/// 一个批次的文件目标始终携带原请求租约，避免异步入队时改用当前活动主机。
struct TransferEnqueueItem {
    name: String,
    request: TransferRequest,
    external_target_lease: Option<ExternalTargetLease>,
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
        let result = match item.external_target_lease.as_ref() {
            Some(lease) => {
                transfers
                    .enqueue_with_external_target(item.request, lease)
                    .await
            }
            None => transfers.enqueue(item.request).await,
        };
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

/// 把逐项反馈压缩为工具栏状态；文本只表示“已入队”，不冒充实际传输完成。
///
/// @author kongweiguang
fn batch_enqueue_message(action_zh: &str, action_en: &str, outcome: BatchEnqueueOutcome) -> String {
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
    message
}

impl FilesPane {
    /// 组合根传入的 Local endpoint 是浏览起点，左右本机栏都从该目录开始而不依赖进程启动目录。
    ///
    /// @author kongweiguang
    pub(crate) fn new(
        files: FileService,
        transfers: TransferService,
        executor: ApplicationTaskExecutor,
        target_name_lookup: FileTargetNameLookup,
        endpoint: FileEndpoint,
    ) -> Self {
        let local = match &endpoint {
            FileEndpoint::Local { path } => FileEndpoint::Local { path: path.clone() },
            _ => FileEndpoint::Local {
                path: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            },
        };
        let target = endpoint;
        Self {
            files,
            transfers,
            executor,
            target_name_lookup,
            target_names: FileTargetNameState::default(),
            panels: [BrowserPanel::new(local), BrowserPanel::new(target)],
            locked_left_host_id: None,
            external_target_lease: None,
            active_side: 1,
            follow_terminal_directory: false,
            current_terminal_endpoint: None,
            preview: None,
            bookmarks: Vec::new(),
            bookmarks_target: None,
            bookmark_request_target: None,
            bookmark_generation: 0,
            transfer_rows: Vec::new(),
            transfer_recovery_rows: Vec::new(),
            queue: transfer_queue::TransferQueueUi::default(),
            host_picker: host_picker::HostPickerState::default(),
            transfer_submission_busy: false,
            permissions: permissions::PermissionEditor::default(),
            compact_layout: false,
            view_mode: view_mode::FilesViewMode::List,
            browser_mode: view_mode::FilesViewMode::List,
            workspace_projection: None,
            workspace_generation: 0,
            workspace_projection_revision: None,
            workspace_scroll: VirtualListScrollHandle::new(),
            show_hidden_files: false,
            status: None,
            status_details_expanded: false,
            initialized: false,
            queue_watch_started: false,
            delete_confirmation: None,
            name_input: None,
            _name_subscription: None,
            name_action: None,
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
        if !compact && self.initialized && self.panels[0].listing.is_none() {
            self.load_panel(0, cx);
        }
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

    /// Root 为独立文件 rail 提供当前终端的可信本机或 SSH 目标；目标失效会撤销旧数据以阻断跨目标操作。
    ///
    /// @author kongweiguang
    pub(crate) fn set_current_terminal_target(
        &mut self,
        endpoint: Option<FileEndpoint>,
        lease: Option<ExternalTargetLease>,
        cx: &mut Context<Self>,
    ) {
        let action = compact_target_update_action(
            self.current_terminal_endpoint.as_ref(),
            &self.panels[1].endpoint,
            endpoint.as_ref(),
            self.follow_terminal_directory,
        );
        if action == CompactTargetUpdateAction::Unavailable {
            if self.compact_target_is_cleared() {
                return;
            }
            self.current_terminal_endpoint = None;
            self.external_target_lease = None;
            self.invalidate_compact_target();
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
                                    self.invalidate_compact_target();
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
        if action == CompactTargetUpdateAction::Unchanged && !target_changed && !lease_changed {
            return;
        }

        self.current_terminal_endpoint = Some(endpoint.clone());
        self.external_target_lease = retained_lease;

        // A filesystem switch binds the exact new endpoint. Local CWD changes never move the
        // user's browser; only a same-host SSH target may follow its directory when opted in.
        let should_notify = match action {
            CompactTargetUpdateAction::BindTarget => {
                self.invalidate_compact_target();
                self.panels[1].endpoint = endpoint.clone();
                self.panels[1].navigation_history.reset(endpoint.clone());
                self.open_endpoint_on_side(1, endpoint, cx);
                true
            }
            CompactTargetUpdateAction::FollowDirectory => {
                self.open_endpoint_on_side(1, endpoint, cx);
                true
            }
            CompactTargetUpdateAction::PreservePath => lease_changed,
            CompactTargetUpdateAction::Unchanged => lease_changed,
            CompactTargetUpdateAction::Unavailable => false,
        };
        if should_notify {
            cx.notify();
        }
    }

    /// 目录跟随只适用于紧凑 SSH 文件栏；本机浏览始终保留手动路径，重开不追溯跳转。
    ///
    /// @author kongweiguang
    pub(crate) fn set_follow_terminal_directory(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.compact_layout
            && enabled
            && !matches!(
                self.current_terminal_endpoint.as_ref(),
                Some(FileEndpoint::Remote { .. })
            )
        {
            return;
        }
        if self.follow_terminal_directory == enabled {
            return;
        }
        self.follow_terminal_directory = enabled;
        if enabled
            && let Some(endpoint) = self.current_terminal_endpoint.clone()
            && self.panels[1].endpoint != endpoint
        {
            self.open_endpoint_on_side(1, endpoint, cx);
        }
        cx.notify();
    }

    /// 按浏览历史回退当前侧目录；不创建新会话，也不改变目标绑定。
    ///
    /// @author kongweiguang
    pub(crate) fn navigate_back(&mut self, side: usize, cx: &mut Context<Self>) {
        let Some(endpoint) = self.panels[side].navigation_history.back() else {
            return;
        };
        self.navigate_to_history_endpoint(side, endpoint, cx);
    }

    /// 按浏览历史前进当前侧目录；不创建新会话，也不改变目标绑定。
    ///
    /// @author kongweiguang
    pub(crate) fn navigate_forward(&mut self, side: usize, cx: &mut Context<Self>) {
        let Some(endpoint) = self.panels[side].navigation_history.forward() else {
            return;
        };
        self.navigate_to_history_endpoint(side, endpoint, cx);
    }

    /// 清除旧目录选择并按历史保存的完整 endpoint 重新读取。
    ///
    /// @author kongweiguang
    fn navigate_to_history_endpoint(
        &mut self,
        side: usize,
        endpoint: FileEndpoint,
        cx: &mut Context<Self>,
    ) {
        self.panels[side].endpoint = endpoint.clone();
        self.panels[side].pending_selection = None;
        self.panels[side].pending_file_preview = false;
        self.panels[side].selected = None;
        self.panels[side].selection.clear();
        self.preview = None;
        self.delete_confirmation = None;
        self.load_panel_at(side, endpoint, cx);
    }

    /// 清除旧文件系统的可见清单、选择、预览及待加载 endpoint，并推进代次拒收迟到回执。
    ///
    /// @author kongweiguang
    fn invalidate_compact_target(&mut self) {
        let panel = &mut self.panels[1];
        panel.generation = panel.generation.wrapping_add(1);
        panel.loading = false;
        panel.requested_endpoint = None;
        panel.listing = None;
        panel.selected = None;
        panel.selection.clear();
        panel.keyboard_focus.clear();
        panel.pending_selection = None;
        panel.pending_file_preview = false;
        panel.invalidate_preview_request();
        panel.tree.cancel_pending_requests();
        self.preview = None;
        self.bookmark_generation = self.bookmark_generation.wrapping_add(1);
        self.bookmarks.clear();
        self.bookmarks_target = None;
        self.bookmark_request_target = None;
        self.delete_confirmation = None;
        self.name_action = None;
        self.status = None;
    }

    /// 避免 Root 每次 render 都重复清理已失效的文件目标和推进请求代次。
    ///
    /// @author kongweiguang
    fn compact_target_is_cleared(&self) -> bool {
        let panel = &self.panels[1];
        self.current_terminal_endpoint.is_none()
            && self.external_target_lease.is_none()
            && !panel.loading
            && panel.listing.is_none()
            && panel.selected.is_none()
            && panel.selection == selection::FileSelection::default()
            && panel.pending_selection.is_none()
            && !panel.pending_file_preview
            && self.preview.is_none()
            && self.bookmarks.is_empty()
            && self.bookmarks_target.is_none()
            && self.bookmark_request_target.is_none()
            && self.delete_confirmation.is_none()
            && self.name_action.is_none()
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
        self.invalidate_compact_target();
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

    /// 将最新 editor 快照按请求代次写入当前工作区视图，拒绝切页后的迟到回包。
    ///
    /// @author kongweiguang
    pub(crate) fn set_workspace_projection(
        &mut self,
        projection: EditorWorkspaceProjection,
        generation: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        if !workspace_projection_request_is_current(
            self.view_mode == view_mode::FilesViewMode::Workspace,
            generation,
            self.workspace_generation,
        ) || !workspace_projection_response_is_fresh(
            self.workspace_projection_revision,
            projection.revision,
        ) {
            return false;
        }
        if self.workspace_projection_revision == Some(projection.revision)
            && self.workspace_projection.as_ref() == Some(&projection)
        {
            return true;
        }
        if self.workspace_projection_revision == Some(projection.revision)
            && self.workspace_projection.is_some()
        {
            return false;
        }
        self.workspace_projection_revision = Some(projection.revision);
        self.workspace_projection = Some(projection);
        self.sync_target_name_requests(cx);
        cx.notify();
        true
    }

    /// Root 将编辑器实际状态推送到可见 Files 工作区，并按版本挡住迟到事件。
    ///
    /// @author kongweiguang
    pub(crate) fn set_live_workspace_projection(
        &mut self,
        projection: EditorWorkspaceProjection,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.view_mode != view_mode::FilesViewMode::Workspace
            || !workspace_projection_push_is_newer(
                self.workspace_projection_revision,
                projection.revision,
            )
        {
            return false;
        }
        self.workspace_projection_revision = Some(projection.revision);
        self.workspace_projection = Some(projection);
        self.sync_target_name_requests(cx);
        cx.notify();
        true
    }

    /// 切换编辑器工作区概览；每次打开都发新代次请求，避免复用过期 tab 快照。
    ///
    /// @author kongweiguang
    pub(super) fn toggle_workspace_overview(&mut self, cx: &mut Context<Self>) {
        if self.view_mode == view_mode::FilesViewMode::Workspace {
            self.view_mode = self.browser_mode;
            self.workspace_projection = None;
        } else {
            self.browser_mode = self.view_mode;
            self.view_mode = view_mode::FilesViewMode::Workspace;
            self.workspace_generation = self.workspace_generation.wrapping_add(1);
            self.workspace_projection = None;
            cx.emit(WorkspaceOverviewRequested {
                generation: self.workspace_generation,
            });
        }
        self.sync_target_name_requests(cx);
        cx.notify();
    }

    /// 切换列表与惰性树时只同步可见 endpoint 选择，不触发额外文件 IO。
    ///
    /// @author kongweiguang
    pub(super) fn toggle_tree_view(&mut self, cx: &mut Context<Self>) {
        if self.view_mode == view_mode::FilesViewMode::Workspace {
            return;
        }
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
            let hidden_count = selected.len().saturating_sub(root_selected.len());
            let selected_names = root_selected
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>();
            self.panels[side].selection.clear();
            self.panels[side].selection.select_all(&selected_names);
            self.panels[side].selected = root_selected.first().cloned();
            self.panels[side].invalidate_preview_request();
            self.preview = None;
            self.view_mode = view_mode::FilesViewMode::List;
            if let Some(entry) = self.panels[side]
                .selected
                .as_ref()
                .filter(|entry| entry.kind == FileEntryKind::File)
                .cloned()
            {
                self.load_preview(side, entry.endpoint, cx);
            }
            self.status = (hidden_count > 0).then(|| {
                locale::text(
                    format!("切回列表后移除了 {hidden_count} 个子目录选择。"),
                    format!("Cleared {hidden_count} nested selections when returning to the list."),
                )
                .to_string()
            });
        }
        self.browser_mode = self.view_mode;
        self.delete_confirmation = None;
        cx.notify();
    }

    /// 将图标视图按钮映射到既有切换用例，确保离开工作区时恢复用户上次的浏览模式。
    ///
    /// @author kongweiguang
    fn set_file_view_mode(&mut self, target: view_mode::FilesViewMode, cx: &mut Context<Self>) {
        if target == view_mode::FilesViewMode::Workspace {
            if self.view_mode != target {
                self.toggle_workspace_overview(cx);
            }
            return;
        }

        if self.view_mode == view_mode::FilesViewMode::Workspace {
            self.toggle_workspace_overview(cx);
        }
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
                let removed = self.panels[side].tree.collapse(&endpoint);
                if removed > 0 {
                    self.status = Some(
                        locale::text(
                            format!("折叠目录后清除了 {removed} 个不可见选择。"),
                            format!(
                                "Cleared {removed} hidden selections from the collapsed folder."
                            ),
                        )
                        .to_string(),
                    );
                }
                if let Some(entry) = self.panels[side].tree.entry(&endpoint)
                    && self.panels[side].selected.as_ref().is_some_and(|selected| {
                        parent_endpoint(&selected.endpoint).as_ref() == Some(&endpoint)
                            || tree::is_descendant_endpoint(&selected.endpoint, &endpoint)
                    })
                {
                    self.panels[side].selected = Some(entry);
                    self.panels[side].invalidate_preview_request();
                    if side == self.active_side {
                        self.preview = None;
                    }
                }
            }
        }
        cx.notify();
    }

    /// GPUI 的焦点行只驱动单项预览；批量选择仍由完整 endpoint 集合单独维护。
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
        if entry.kind == FileEntryKind::File {
            self.load_preview(side, endpoint, cx);
        } else {
            self.panels[side].invalidate_preview_request();
            if side == self.active_side {
                self.preview = None;
            }
        }
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
        self.panels[side].selected = focused.clone();
        self.preview = None;
        if let Some(entry) = focused.filter(|entry| entry.kind == FileEntryKind::File) {
            self.load_preview(side, entry.endpoint, cx);
        } else {
            self.panels[side].invalidate_preview_request();
        }
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
                self.host_picker.right_target_unselected,
                &self.panels[0].endpoint,
                self.panels[0].navigation_history.entries.len(),
                self.locked_left_host_id.is_some(),
            );
        self.host_picker.right_target_unselected = false;
        self.host_picker.expanded = false;
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
        self.open_endpoint(endpoint, cx);
    }

    /// 切换活动面板时复用目标级书签缓存，并取消上一侧尚未完成的预览。
    ///
    /// @author kongweiguang
    fn activate_side(&mut self, side: usize, cx: &mut Context<Self>) {
        if self.active_side == side {
            return;
        }
        self.panels[self.active_side].invalidate_preview_request();
        self.active_side = side;
        self.preview = None;
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
        self.open_endpoint_on_side_with_selection(side, endpoint, None, false, cx);
    }

    /// 将 endpoint 与可选恢复项一次写入面板，再发起目录读取，避免响应快于选择设置。
    ///
    /// @author kongweiguang
    fn open_endpoint_on_side_with_selection(
        &mut self,
        side: usize,
        endpoint: FileEndpoint,
        pending_selection: Option<String>,
        pending_file_preview: bool,
        cx: &mut Context<Self>,
    ) {
        if side == 1 && self.host_picker.enabled {
            self.host_picker.right_target_unselected = false;
            self.host_picker.expanded = false;
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
        if self.active_side != side {
            self.panels[self.active_side].invalidate_preview_request();
            self.preview = None;
        }
        self.active_side = side;
        self.panels[side].pending_selection = pending_selection;
        self.panels[side].pending_file_preview = pending_file_preview;
        self.panels[side].invalidate_preview_request();
        self.panels[side].navigation_history.push(endpoint.clone());
        self.delete_confirmation = None;
        self.load_panel_at(side, endpoint, cx);
        cx.notify();
    }

    /// 打开主机本地或 SSH 配置文件时先读取父目录并验证真实文件项，再复用普通预览路径。
    ///
    /// @author kongweiguang
    pub fn open_endpoint_with_selection(
        &mut self,
        endpoint: FileEndpoint,
        selection_name: Option<String>,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        if matches!(&endpoint, FileEndpoint::Container { .. }) {
            let error = AppError::unsupported("Compose 配置预览只支持主机本机或 SSH 文件 endpoint");
            self.status = Some(localized_error(&error));
            cx.notify();
            return Err(error);
        }
        let expected_name = endpoint_name(&endpoint)
            .ok_or_else(|| AppError::invalid("文件 endpoint 缺少可显示的文件名"))?;
        if selection_name
            .as_deref()
            .is_some_and(|name| name != expected_name)
        {
            return Err(AppError::invalid("选择名称与文件 endpoint 不匹配"));
        }
        let parent = parent_endpoint(&endpoint)
            .ok_or_else(|| AppError::invalid("文件 endpoint 没有可读取的父目录"))?;
        let side = if endpoint_same_filesystem(&self.panels[0].endpoint, &endpoint) {
            0
        } else if endpoint_same_filesystem(&self.panels[1].endpoint, &endpoint) {
            1
        } else if matches!(endpoint, FileEndpoint::Local { .. }) {
            0
        } else {
            1
        };
        if side == 0 && !endpoint_matches_locked_host(&parent, self.locked_left_host_id.as_ref()) {
            let error = AppError::invalid("Compose 文件不属于左侧锁定的主机");
            self.status = Some(localized_error(&error));
            cx.notify();
            return Err(error);
        }
        if expected_name.starts_with('.') {
            self.show_hidden_files = true;
        }
        self.open_endpoint_on_side_with_selection(side, parent, Some(expected_name), true, cx);
        Ok(())
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

    /// Enter 提交当前 endpoint 的路径草稿；错误保留草稿与旧列表，供用户修正或取消。
    ///
    /// @author kongweiguang
    fn commit_path_edit(&mut self, side: usize, cx: &mut Context<Self>) {
        let Some(input) = self.panels[side].path_input.as_ref().cloned() else {
            return;
        };
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

        if self.active_side != side {
            self.panels[self.active_side].invalidate_preview_request();
            self.preview = None;
        }
        self.active_side = side;
        self.panels[side].pending_selection = None;
        self.panels[side].selected = None;
        self.panels[side].selection.clear();
        self.panels[side].invalidate_preview_request();
        self.preview = None;
        self.delete_confirmation = None;
        self.status = None;
        self.panels[side].navigation_history.push(endpoint.clone());
        self.load_panel_at(side, endpoint, cx);
        cx.notify();
    }

    /// 在当前 endpoint 上切换点文件显示，并清除因此变为不可见的选择和预览任务。
    ///
    /// @author kongweiguang
    fn toggle_hidden_files(&mut self, cx: &mut Context<Self>) {
        self.show_hidden_files = !self.show_hidden_files;
        let mut removed_tree_selections = 0usize;
        for panel in &mut self.panels {
            removed_tree_selections += panel.tree.retain_visible_selection(self.show_hidden_files);
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
                    self.panels[side].invalidate_preview_request();
                    if side == self.active_side {
                        self.preview = None;
                    }
                }
            }
        }
        if removed_tree_selections > 0 {
            self.status = Some(
                locale::text(
                    format!("隐藏文件后清除了 {removed_tree_selections} 个树选择。"),
                    format!(
                        "Cleared {removed_tree_selections} tree selections hidden by the filter."
                    ),
                )
                .to_string(),
            );
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

    /// 更新选择并让预览跟随稳定身份；新选择先撤销旧补滚动，键盘移动可随后重建受代次保护的意图。
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
            self.panels[self.active_side]
                .keyboard_scroll_intent
                .cancel();
            self.panels[self.active_side].invalidate_preview_request();
            self.preview = None;
            self.active_side = side;
            self.load_bookmarks(side, cx);
        }
        self.panels[side].keyboard_scroll_intent.cancel();
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
        self.panels[side].selected = focused_entry.clone();
        self.preview = None;
        if let Some(entry) = focused_entry.filter(|entry| entry.kind == FileEntryKind::File) {
            self.load_preview(side, entry.endpoint, cx);
        } else {
            self.panels[side].invalidate_preview_request();
        }
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

    /// 校验目标后按当前 viewport 移动；文件预览回流后只补滚一次，且鼠标浏览可撤销该意图。
    ///
    /// @author kongweiguang
    fn move_list_keyboard_focus(
        &mut self,
        side: usize,
        generation: u64,
        directory: &FileEndpoint,
        ordered_names: &[String],
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) {
        let movement = match event.keystroke.key.to_ascii_lowercase().as_str() {
            "up" => FileListMovement::Previous,
            "down" => FileListMovement::Next,
            "pageup" => FileListMovement::PagePrevious {
                visible_rows: self.panels.get(side).map_or(0, |panel| {
                    (f32::from(panel.list_scroll.bounds().size.height) / FILE_LIST_ROW_HEIGHT_PX)
                        .floor() as usize
                }),
            },
            "pagedown" => FileListMovement::PageNext {
                visible_rows: self.panels.get(side).map_or(0, |panel| {
                    (f32::from(panel.list_scroll.bounds().size.height) / FILE_LIST_ROW_HEIGHT_PX)
                        .floor() as usize
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
        let target_name = entry.name.clone();
        let target_endpoint = entry.endpoint.clone();
        let wait_for_preview = entry.kind == FileEntryKind::File;

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
        let panel = &mut self.panels[side];
        let selected_is_keyboard_target = panel
            .selected
            .as_ref()
            .is_some_and(|selected| selected.endpoint == target_endpoint);
        if wait_for_preview && selected_is_keyboard_target {
            let panel_generation = panel.generation;
            let preview_generation = panel.preview_generation;
            panel.keyboard_scroll_intent.arm(
                panel_generation,
                directory,
                &target_name,
                &target_endpoint,
                preview_generation,
                true,
            );
        } else {
            panel.keyboard_scroll_intent.cancel();
        }
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
            self.panels[side].invalidate_preview_request();
            self.preview = None;
            if let Some(entry) = self.panels[side]
                .selected
                .as_ref()
                .filter(|entry| entry.kind == FileEntryKind::File)
                .cloned()
            {
                self.load_preview(side, entry.endpoint, cx);
            }
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
            self.panels[side].invalidate_preview_request();
            self.preview = None;
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
                self.panels[side].selected = Some(entry.clone());
                if entry.kind == FileEntryKind::File {
                    self.load_preview(side, entry.endpoint, cx);
                }
            }
        }
        cx.notify();
    }

    /// Enter 与双击打开目录或请求 Root 在编辑器中打开文件，单击只改变选择和预览。
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
        self.panels[side].invalidate_preview_request();
        self.preview = None;
        cx.emit(OpenFileRequested {
            endpoint: entry.endpoint,
        });
        cx.notify();
    }

    /// 打开传输目标所在目录并选中目标项，确保队列中的完整 endpoint 可被定位。
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
        let selection = endpoint_name(&destination);
        self.open_endpoint_on_side_with_selection(side, parent, selection, false, cx);
    }

    /// 恢复一个独立的双面板传输工作区，并校验左侧锁定主机与初始选择。
    ///
    /// @author kongweiguang
    pub fn open_workspace_transfer(
        &mut self,
        left: FileEndpoint,
        right: FileEndpoint,
        locked_left_host_id: Option<HostId>,
        initial_right_selection: Option<String>,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        if !endpoint_matches_locked_host(&left, locked_left_host_id.as_ref()) {
            return Err(AppError::invalid("左侧文件 endpoint 与锁定主机不匹配"));
        }

        self.initialized = true;
        self.host_picker.right_target_unselected = false;
        self.host_picker.expanded = false;
        self.locked_left_host_id = locked_left_host_id;
        self.panels[0].endpoint = left;
        let left_endpoint = self.panels[0].endpoint.clone();
        self.panels[0].navigation_history.reset(left_endpoint);
        self.panels[0].pending_selection = None;
        self.panels[0].pending_file_preview = false;
        self.panels[0].selected = None;
        self.panels[0].selection.clear();
        self.panels[1].endpoint = right;
        let right_endpoint = self.panels[1].endpoint.clone();
        self.panels[1].navigation_history.reset(right_endpoint);
        self.panels[1].pending_selection = initial_right_selection;
        self.panels[1].pending_file_preview = false;
        self.panels[1].selected = None;
        self.panels[1].selection.clear();
        self.sync_target_name_requests(cx);
        self.preview = None;
        self.bookmarks.clear();
        self.bookmarks_target = None;
        self.bookmark_request_target = None;
        self.bookmark_generation = self.bookmark_generation.wrapping_add(1);
        self.status = None;
        self.delete_confirmation = None;
        self.name_action = None;
        self.active_side = 1;
        self.external_target_lease = None;
        self.load_panel(0, cx);
        self.load_panel(1, cx);
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
        if !self.host_picker.right_target_unselected
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
        self.refresh_panel(0, cx);
        if !self.host_picker.right_target_unselected {
            self.refresh_panel(1, cx);
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

    /// Render 首次取得真实 Window 后创建 Kit 输入框，构造器保持可由 composition root 直接调用。
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
        let subscription = cx.subscribe(&input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
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

    /// 开始创建目录并使用相同经过名称校验的输入框，减少临时弹层和重复状态。
    fn prepare_create_directory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_name_input(window, cx);
        if let Some(input) = &self.name_input {
            input.update(cx, |state, cx| state.set_value(String::new(), window, cx));
        }
        self.name_action = Some(NameAction::CreateDirectory);
        self.active_side = self.active_side.min(1);
        cx.notify();
    }

    /// 开始重命名时预填当前文件名；路径的父目录由 endpoint 类型保留。
    fn prepare_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_name_input(window, cx);
        let name = self.panels[self.active_side]
            .selected
            .as_ref()
            .and_then(|entry| endpoint_name(&entry.endpoint))
            .unwrap_or_default();
        if let Some(input) = &self.name_input {
            input.update(cx, |state, cx| state.set_value(name, window, cx));
        }
        self.name_action = Some(NameAction::Rename);
        cx.notify();
    }

    /// 验证名称为单一目录项后调用 FileService，避免 UI 输入成为父路径或绝对路径。
    ///
    /// @author kongweiguang
    fn commit_name_action(&mut self, cx: &mut Context<Self>) {
        let Some(action) = self.name_action else {
            return;
        };
        let Some(input) = &self.name_input else {
            return;
        };
        let name = input.update(cx, |state, _| state.value().to_string());
        if !valid_item_name(&name) {
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
        let side = self.active_side;
        let files = self.files.clone();
        let executor = self.executor.clone();
        let external_target_lease =
            self.external_target_lease_for_endpoint(&self.panels[side].endpoint);
        match action {
            NameAction::CreateDirectory => {
                let Some(destination) = child_endpoint(&self.panels[side].endpoint, &name) else {
                    self.status = Some(
                        locale::text(
                            "当前目录不支持此名称。",
                            "This directory does not support that name.",
                        )
                        .to_string(),
                    );
                    cx.notify();
                    return;
                };
                let external_target_lease = external_target_lease.clone();
                cx.spawn(async move |this, cx| {
                    let result = executor
                        .run(move |cancel| async move {
                            let _external_target_scope = external_target_lease;
                            files.create_directory(cancel, destination).await
                        })
                        .await;
                    cx.update(|app| {
                        let _ = this.update(app, |pane, cx| {
                            pane.name_action = None;
                            match result {
                                Ok(()) => {
                                    pane.status = Some(
                                        locale::text("目录已创建。", "Folder created.").to_string(),
                                    )
                                }
                                Err(error) => pane.status = Some(localized_error(&error)),
                            }
                            pane.load_panel(side, cx);
                            cx.notify();
                        });
                    });
                })
                .detach();
            }
            NameAction::Rename => {
                let Some(source) = self.panels[side]
                    .selected
                    .as_ref()
                    .map(|entry| entry.endpoint.clone())
                else {
                    self.status = Some(
                        locale::text(
                            "请先选择要重命名的文件或目录。",
                            "Select a file or folder to rename.",
                        )
                        .to_string(),
                    );
                    cx.notify();
                    return;
                };
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
                cx.spawn(async move |this, cx| {
                    let result = executor
                        .run(move |cancel| async move {
                            let _external_target_scope = external_target_lease;
                            files.rename(cancel, source, destination).await
                        })
                        .await;
                    cx.update(|app| {
                        let _ = this.update(app, |pane, cx| {
                            pane.name_action = None;
                            match result {
                                Ok(()) => {
                                    pane.status = Some(
                                        locale::text("项目已重命名。", "Item renamed.").to_string(),
                                    )
                                }
                                Err(error) => pane.status = Some(localized_error(&error)),
                            }
                            pane.load_panel(side, cx);
                            cx.notify();
                        });
                    });
                })
                .detach();
            }
        }
    }

    /// 对所选项目执行统一二次确认，目录是否可删由 FileService 按协议规则判断。
    ///
    /// @author kongweiguang
    fn delete_selected(&mut self, cx: &mut Context<Self>) {
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
        let confirmed_endpoints = entries
            .iter()
            .map(|entry| entry.endpoint.clone())
            .collect::<Vec<_>>();
        if self.delete_confirmation.as_ref() != Some(&confirmed_endpoints) {
            self.delete_confirmation = Some(confirmed_endpoints);
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
                    pane.status = Some(match result {
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
                            message
                        }
                        Err(error) => localized_error(&error),
                    });
                    pane.load_panel(side, cx);
                    pane.preview = None;
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// 根层模态只读取冻结的完整 endpoint，不从当前 Tab 或主机名猜测危险操作目标。
    ///
    /// @author kongweiguang
    pub(crate) fn pending_delete_confirmation(&self) -> Option<Vec<FileEndpoint>> {
        self.delete_confirmation.clone()
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

    /// 使用 GPUI 原生选择器并逐项交给应用层检查；部分失败和取消均返回准确计数。
    ///
    /// @author kongweiguang
    fn prompt_upload(&mut self, cx: &mut Context<Self>) {
        if !self.begin_transfer_submission(cx) {
            return;
        }
        let destination_directory = self.panels[self.active_side].endpoint.clone();
        let conflict_policy = self.queue.conflict_policy;
        let external_target_lease = self.external_target_lease_for_endpoint(&destination_directory);
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some(locale::text(
                "选择要传入当前目录的文件或目录",
                "Choose files or folders to upload here",
            )),
        });
        let files = self.files.clone();
        let transfers = self.transfers.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    let selected = selected
                        .await
                        .map_err(|_| AppError::cancelled("文件选择已取消"))?
                        .map_err(|_| AppError::infrastructure("系统文件选择器无法打开"))?;
                    let Some(paths) = selected else {
                        return Ok((0, 0, Vec::new()));
                    };
                    let total = paths.len();
                    let mut queued = 0;
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
                        let enqueue_result = match external_target_lease.as_ref() {
                            Some(lease) => {
                                transfers.enqueue_with_external_target(request, lease).await
                            }
                            None => transfers.enqueue(request).await,
                        };
                        if let Err(error) = enqueue_result {
                            issues.push(format!("{name}: {}", localized_error(&error)));
                            continue;
                        }
                        queued += 1;
                    }
                    Ok((queued, total, issues))
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    pane.end_transfer_submission();
                    pane.status = match result {
                        Ok((0, 0, _)) => Some(locale::text("已取消文件选择。", "File selection cancelled.").to_string()),
                        Ok((queued, total, issues)) if issues.is_empty() => {
                            Some(format!(
                                "{} {queued}/{total}.",
                                locale::text("已加入上传任务", "Upload tasks queued")
                            ))
                        }
                        Ok((queued, total, issues)) => Some(format!(
                            "{} {queued}/{total}; {}: {}",
                            locale::text("已加入上传任务", "Upload tasks queued"),
                            locale::text("未完成项", "unfinished items"),
                            issues.join(&locale::text("；", "; "))
                        )),
                        Err(error) => Some(localized_error(&error)),
                    };
                    pane.reload_transfers(cx);
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// 对单文件保留系统保存对话框；多项或目录选择一个本机父目录后逐项入同一队列。
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

        let total = selected.len();
        let total = total + included_by_selected_directories;
        let conflict_policy = self.queue.conflict_policy;
        let mut sources = Vec::new();
        let mut issues = Vec::new();
        for entry in selected {
            let Some(name) = endpoint_name(&entry.endpoint).filter(|name| valid_item_name(name))
            else {
                issues.push(
                    locale::text(
                        format!("{}: 无法安全读取文件名。", entry.name),
                        format!("{}: the name cannot be represented safely.", entry.name),
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
            self.status = Some(batch_enqueue_message(
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
            ));
            cx.notify();
            return;
        }

        if !self.begin_transfer_submission(cx) {
            return;
        }

        let target_directory = match &self.panels[0].endpoint {
            FileEndpoint::Local { path } => path.clone(),
            _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        let transfers = self.transfers.clone();
        let executor = self.executor.clone();
        if sources.len() == 1 && sources[0].kind == TransferKind::File && total == 1 {
            let Some(source) = sources.pop() else {
                self.status = Some(locale::text(
                    "所选下载项已失效，请刷新目录后重试。",
                    "The selected download item is no longer available. Refresh the directory and try again.",
                ).to_string());
                cx.notify();
                return;
            };
            let selected_path = cx.prompt_for_new_path(&target_directory, Some(&source.name));
            cx.spawn(async move |this, cx| {
                let result = executor
                    .run(move |cancel| async move {
                        let path = selected_path
                            .await
                            .map_err(|_| AppError::cancelled("保存位置选择已取消"))?
                            .map_err(|_| AppError::infrastructure("系统保存对话框无法打开"))?;
                        let Some(path) = path else { return Ok(None) };
                        let request = TransferRequest {
                            source: source.source,
                            destination: FileEndpoint::Local { path },
                            kind: source.kind,
                            conflict_policy,
                            idle_timeout_seconds: None,
                        };
                        let item = TransferEnqueueItem {
                            name: source.name,
                            external_target_lease: source.external_target_lease,
                            request,
                        };
                        Ok(Some(
                            enqueue_transfer_items(transfers, cancel, vec![item], 1, 0, issues)
                                .await,
                        ))
                    })
                    .await;
                cx.update(|app| {
                    let _ = this.update(app, |pane, cx| {
                        pane.end_transfer_submission();
                        pane.status = match result {
                            Ok(Some(outcome)) => Some(batch_enqueue_message(
                                "下载任务已入队",
                                "Download tasks queued",
                                outcome,
                            )),
                            Ok(None) => Some(
                                locale::text(
                                    "已取消下载位置选择。",
                                    "Download location selection cancelled.",
                                )
                                .to_string(),
                            ),
                            Err(error) => Some(localized_error(&error)),
                        };
                        pane.reload_transfers(cx);
                        cx.notify();
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
                .run(move |cancel| async move {
                    let paths = selected_path
                        .await
                        .map_err(|_| AppError::cancelled("下载目录选择已取消"))?
                        .map_err(|_| AppError::infrastructure("系统目录选择器无法打开"))?;
                    let Some(mut paths) = paths else {
                        return Ok(None);
                    };
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
                    Ok(Some(
                        enqueue_transfer_items(
                            transfers,
                            cancel,
                            items,
                            total,
                            included_by_selected_directories,
                            issues,
                        )
                        .await,
                    ))
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    pane.end_transfer_submission();
                    pane.status = match result {
                        Ok(Some(outcome)) => Some(batch_enqueue_message(
                            "下载任务已入队",
                            "Download tasks queued",
                            outcome,
                        )),
                        Ok(None) => Some(
                            locale::text(
                                "已取消下载位置选择。",
                                "Download location selection cancelled.",
                            )
                            .to_string(),
                        ),
                        Err(error) => Some(localized_error(&error)),
                    };
                    pane.reload_transfers(cx);
                    cx.notify();
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

    /// 新目录请求先撤销键盘可见性补偿，再重置滚动；仅匹配 generation 的回执能提交新清单。
    ///
    /// @author kongweiguang
    fn load_panel_at(&mut self, side: usize, endpoint: FileEndpoint, cx: &mut Context<Self>) {
        self.panels[side].keyboard_scroll_intent.cancel();
        self.panels[side]
            .keyboard_focus
            .reset_for_directory(&endpoint);
        self.panels[side].list_scroll = VirtualListScrollHandle::new();
        let previous_preview = (self.active_side == side)
            .then(|| self.preview.clone())
            .flatten();
        self.panels[side].invalidate_preview_request();
        if self.active_side == side {
            self.preview = None;
        }
        let generation = {
            let panel = &mut self.panels[side];
            begin_panel_load_request(
                &mut panel.generation,
                &mut panel.loading,
                &mut panel.requested_endpoint,
                &endpoint,
            )
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
                .run(move |cancel| async move {
                    let _external_target_scope = external_target_lease;
                    files.list_directory(cancel, endpoint).await
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    let locked_left_host_id = pane.locked_left_host_id.clone();
                    let panel = &mut pane.panels[side];
                    if !finish_panel_load_request(
                        panel.generation,
                        generation,
                        &mut panel.requested_endpoint,
                        &requested_endpoint,
                    ) {
                        return;
                    }
                    panel.loading = false;
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
                            let tree_selection_dropped = pane.panels[side].tree.set_root_listing(
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
                                .filter(|entry| {
                                    pane.panels[side].entry_filter.matches(entry.kind)
                                })
                                .map(|entry| entry.name.clone())
                                .collect::<Vec<_>>();
                            let panel = &mut pane.panels[side];
                            if panel.endpoint != listing.directory {
                                panel.selection.clear();
                                panel.selected = None;
                            }
                            panel.endpoint = listing.directory.clone();
                            let requested_selection = panel.pending_selection.take();
                            let selected = requested_selection.as_ref().and_then(|name| {
                                listing
                                    .entries
                                    .iter()
                                    .find(|entry| &entry.name == name)
                                    .cloned()
                            });
                            let file_preview_requested = panel.pending_file_preview;
                            panel.pending_file_preview = false;
                            if let Some(entry) = selected.as_ref() {
                                panel.selection.select_only(&entry.name);
                            }
                            panel.selection.retain_visible(&visible_names);
                            panel
                                .keyboard_focus
                                .retain_visible(&listing.directory, &keyboard_visible_names);
                            if file_preview_requested
                                && selected
                                    .as_ref()
                                    .is_some_and(render::browser::is_hidden_entry)
                            {
                                pane.show_hidden_files = true;
                                if let Some(entry) = selected.as_ref() {
                                    panel.selection.select_only(&entry.name);
                                }
                            }
                            if file_preview_requested
                                && !selected
                                    .as_ref()
                                    .is_some_and(|entry| entry.kind == FileEntryKind::File)
                            {
                                panel.selected = None;
                                pane.status = Some(locale::text(
                                    "指定的配置文件不存在或不是普通文件。",
                                    "The requested configuration file is missing or not a regular file.",
                                ).to_string());
                            } else if requested_selection.is_some() && selected.is_none() {
                                panel.selected = None;
                                pane.status = Some(
                                    locale::text(
                                        "恢复的选择项当前不可用。",
                                        "The restored selection is no longer available.",
                                    )
                                    .to_string(),
                                );
                            } else {
                                panel.selected = selected;
                                pane.status = None;
                            }
                            let preview_endpoint = panel
                                .selected
                                .as_ref()
                                .filter(|entry| entry.kind == FileEntryKind::File)
                                .map(|entry| entry.endpoint.clone());
                            panel.listing = Some(listing);
                            if tree_selection_dropped > 0 && pane.status.is_none() {
                                pane.status = Some(locale::text(
                                    format!(
                                        "目录刷新后清除了 {tree_selection_dropped} 个已失效树选择。"
                                    ),
                                    format!(
                                        "Cleared {tree_selection_dropped} stale tree selections after refresh."
                                    ),
                                ).to_string());
                            }
                            if side == pane.active_side {
                                pane.load_bookmarks(side, cx);
                                if let Some(endpoint) = preview_endpoint {
                                    pane.load_preview(side, endpoint, cx);
                                }
                            }
                        }
                        Err(error) => {
                            pane.panels[side].pending_selection = None;
                            pane.panels[side].pending_file_preview = false;
                            pane.status = Some(localized_error(&error));
                            if side == pane.active_side {
                                pane.preview = previous_preview;
                            }
                        }
                    }
                    pane.sync_target_name_requests(cx);
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 同步双面板和当前 editor 投影目标集合；同一 HostId 或容器身份只查询一次。
    ///
    /// @author kongweiguang
    fn sync_target_name_requests(&mut self, cx: &mut Context<Self>) {
        let mut endpoints = self
            .panels
            .iter()
            .map(|panel| panel.endpoint.clone())
            .collect::<Vec<_>>();
        if let Some(projection) = &self.workspace_projection {
            endpoints.extend(projection.tabs.iter().map(|tab| tab.endpoint.clone()));
        }
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
            return;
        };
        if self.bookmarks_target.as_ref() == Some(&target) {
            if self.bookmark_request_target.is_some() {
                self.bookmark_generation = self.bookmark_generation.wrapping_add(1);
                self.bookmark_request_target = None;
            }
            return;
        }
        if self.bookmark_request_target.as_ref() == Some(&target) {
            return;
        }
        self.bookmark_generation = self.bookmark_generation.wrapping_add(1);
        let generation = self.bookmark_generation;
        self.bookmark_request_target = Some(target.clone());
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
                        }
                        Err(error) => pane.status = Some(localized_error(&error)),
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// 目录项进入目录，普通文件只加载有界预览并保留显式编辑入口。
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
        if switched_side {
            self.panels[self.active_side].invalidate_preview_request();
            self.preview = None;
        }
        self.active_side = side;
        if entry.kind == FileEntryKind::Directory {
            self.panels[side]
                .navigation_history
                .push(entry.endpoint.clone());
            self.panels[side].endpoint = entry.endpoint;
            self.panels[side].pending_selection = None;
            self.panels[side].selected = None;
            self.panels[side].selection.clear();
            self.preview = None;
            self.load_panel(side, cx);
            return;
        }
        if switched_side {
            self.load_bookmarks(side, cx);
        }
        if let Some(name) = endpoint_name(&entry.endpoint) {
            self.panels[side].selection.select_only(&name);
        }
        self.panels[side].selected = Some(entry.clone());
        self.preview = None;
        if entry.kind == FileEntryKind::File {
            self.load_preview(side, entry.endpoint, cx);
        } else {
            self.panels[side].invalidate_preview_request();
        }
        cx.notify();
    }

    /// 预览按面板、选择与请求代次提交结果；旧读取在选项或 pane 失效时会被取消。
    ///
    /// @author kongweiguang
    fn load_preview(&mut self, side: usize, endpoint: FileEndpoint, cx: &mut Context<Self>) {
        self.panels[side].invalidate_preview_request();
        let generation = self.panels[side].preview_generation;
        let request_cancellation = Arc::new(PreviewTaskCancellation::default());
        self.panels[side].preview_cancellation = Some(Arc::clone(&request_cancellation));
        let external_target_lease = self.external_target_lease_for_endpoint(&endpoint);
        let files = self.files.clone();
        let executor = self.executor.clone();
        let expected_endpoint = endpoint.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    request_cancellation.bind_task_token(cancel.clone());
                    let _external_target_scope = external_target_lease;
                    let result = files
                        .preview_file(cancel, endpoint, MAX_FILE_PREVIEW_BYTES)
                        .await;
                    request_cancellation.clear_task_token();
                    result
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    let active_side = pane.active_side;
                    let panel = &mut pane.panels[side];
                    let request_is_current = panel.preview_generation == generation
                        && panel.selected.as_ref().is_some_and(|selected| {
                            selected.kind == FileEntryKind::File
                                && selected.endpoint == expected_endpoint
                        });
                    if !request_is_current || active_side != side {
                        return;
                    }
                    panel.preview_cancellation = None;
                    match result {
                        Ok(preview) => pane.preview = Some(preview),
                        Err(error) => pane.status = Some(error.public_message().to_owned()),
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// 返回一层目录的父路径；POSIX 远端路径不经过 Windows Path 规则解释。
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
        self.panels[side].endpoint = endpoint;
        self.panels[side].pending_selection = None;
        self.panels[side].pending_file_preview = false;
        self.panels[side].selected = None;
        self.panels[side].selection.clear();
        self.preview = None;
        self.load_panel(side, cx);
    }

    /// 将指定面板的当前路径加入或移除目标级书签；回执按面板端点校验，切换活动侧不丢结果。
    ///
    /// @author kongweiguang
    fn toggle_bookmark(&mut self, side: usize, cx: &mut Context<Self>) {
        let endpoint = self.panels[side].endpoint.clone();
        let external_target_lease = self.external_target_lease_for_endpoint(&endpoint);
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
        self.bookmark_generation = self.bookmark_generation.wrapping_add(1);
        let generation = self.bookmark_generation;
        self.bookmark_request_target = None;
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
                    files.set_bookmark(cancel, bookmark, !bookmarked).await
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
                        }
                        Err(error) => pane.status = Some(localized_error(&error)),
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
        let source_side = self.active_side;
        let destination_side = 1 - source_side;
        if !can_transfer_to_other_side(
            self.host_picker.enabled,
            self.host_picker.right_target_unselected,
            source_side,
        ) {
            self.status = Some(
                locale::text("请先选择右侧主机", "Select a host on the right first").to_string(),
            );
            cx.notify();
            return;
        }
        let (selected, included_by_selected_directories) =
            if self.view_mode == view_mode::FilesViewMode::Tree {
                let plan = self.panels[source_side]
                    .tree
                    .transfer_selection_plan(self.show_hidden_files);
                (plan.roots, plan.included_descendants)
            } else {
                (self.selected_entries(source_side), 0)
            };
        if selected.is_empty() {
            self.status = Some(
                locale::text("请先选择文件或目录。", "Select files or folders first.").to_string(),
            );
            cx.notify();
            return;
        }

        let total = selected.len() + included_by_selected_directories;
        let destination_directory = self.panels[destination_side].endpoint.clone();
        let conflict_policy = self.queue.conflict_policy;
        let mut items = Vec::new();
        let mut issues = Vec::new();
        for entry in selected {
            let Some(name) = endpoint_name(&entry.endpoint).filter(|name| valid_item_name(name))
            else {
                issues.push(
                    locale::text(
                        format!("{}: 无法安全读取文件名。", entry.name),
                        format!("{}: the name cannot be represented safely.", entry.name),
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
                            format!("{}: 当前传输不跟随符号链接或特殊文件。", entry.name),
                            format!(
                                "{}: symbolic links and special files are not transferred.",
                                entry.name
                            ),
                        )
                        .to_string(),
                    );
                    continue;
                }
            };
            let Some(destination) = child_endpoint(&destination_directory, &name) else {
                issues.push(
                    locale::text(
                        format!("{}: 目标文件系统不支持该名称。", name),
                        format!("{}: the destination does not support this name.", name),
                    )
                    .to_string(),
                );
                continue;
            };
            let request = TransferRequest {
                source: entry.endpoint,
                destination,
                kind,
                conflict_policy,
                idle_timeout_seconds: None,
            };
            items.push(TransferEnqueueItem {
                name,
                external_target_lease: self.external_target_lease_for_request(&request),
                request,
            });
        }
        self.enqueue_transfer_batch(
            items,
            total,
            included_by_selected_directories,
            issues,
            BatchEnqueueLabels {
                action_zh: "复制任务已入队",
                action_en: "Copy tasks queued",
            },
            cx,
        );
    }

    /// 将批次逐项交给唯一 TransferService；取消会保留已有受理结果且不会伪造传输完成。
    ///
    /// @author kongweiguang
    fn enqueue_transfer_batch(
        &mut self,
        items: Vec<TransferEnqueueItem>,
        total: usize,
        included_by_selected_directories: usize,
        issues: Vec<String>,
        labels: BatchEnqueueLabels,
        cx: &mut Context<Self>,
    ) {
        if !self.begin_transfer_submission(cx) {
            return;
        }
        let transfers = self.transfers.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    Ok(enqueue_transfer_items(
                        transfers,
                        cancel,
                        items,
                        total,
                        included_by_selected_directories,
                        issues,
                    )
                    .await)
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    pane.end_transfer_submission();
                    match result {
                        Ok(outcome) => {
                            pane.status = Some(batch_enqueue_message(
                                labels.action_zh,
                                labels.action_en,
                                outcome,
                            ))
                        }
                        Err(error) => pane.status = Some(localized_error(&error)),
                    }
                    pane.reload_transfers(cx);
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// 同一文件面板一次只允许一个传输批次进入系统选择器或队列，防止连点生成重复任务。
    ///
    /// @author kongweiguang
    fn begin_transfer_submission(&mut self, cx: &mut Context<Self>) -> bool {
        if self.transfer_submission_busy {
            self.status = Some(
                locale::text(
                    "传输任务正在提交，请稍候。",
                    "A transfer batch is being submitted. Please wait.",
                )
                .to_string(),
            );
            cx.notify();
            return false;
        }
        self.transfer_submission_busy = true;
        self.status =
            Some(locale::text("正在准备传输任务…", "Preparing transfer tasks…").to_string());
        cx.notify();
        true
    }

    /// 对话框取消、队列受理或执行失败都要释放面板闩锁，允许用户明确重试。
    ///
    /// @author kongweiguang
    fn end_transfer_submission(&mut self) {
        self.transfer_submission_busy = false;
    }

    /// 队列快照变化仅重读新提交回执对应的当前目标目录，不改变用户的在途导航意图。
    /// 读取仍经过 panel generation 门禁，旧路径或迟到回执不能覆盖新目录。
    ///
    /// @author kongweiguang
    fn apply_transfer_rows(&mut self, rows: Vec<TransferSummary>, cx: &mut Context<Self>) {
        let refresh_directories =
            completed_transfer_refresh_directories(&self.transfer_rows, &rows);
        self.transfer_rows = rows;
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

    /// 所有活队列读取共用代次，只接纳最新的任务与恢复材料，避免归档前快照迟到复活。
    /// 首次另读一页冷档以保留历史入口，进度广播不会反复扫描冷历史；启动时通知等待态。
    ///
    /// @author kongweiguang
    fn reload_transfers(&mut self, cx: &mut Context<Self>) {
        let generation = self.queue.begin_transfer_refresh();
        cx.notify();
        if !self.queue.archive_loaded && !self.queue.archive_loading {
            self.load_transfer_archive_page(None, cx);
        }
        let transfers = self.transfers.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    let rows = transfers.list().await;
                    let recovery = transfers.list_recovery(cancellation).await;
                    Ok((rows, recovery))
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    if !pane.queue.accepts_transfer_refresh(generation) {
                        return;
                    }
                    match result {
                        Ok((rows, recovery)) => {
                            pane.apply_transfer_rows(rows, cx);
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
                        let wake = waiting_events.recv(cancel).await;
                        Ok((wake, waiting_events))
                    })
                    .await;
                let (wake, next_events) = match wake {
                    Ok(result) => result,
                    Err(_) => break,
                };
                events = next_events;
                if wake.is_none() {
                    break;
                }
                let generation = cx.update(|app| {
                    this.update(app, |pane, cx| {
                        let generation = pane.queue.begin_transfer_refresh();
                        cx.notify();
                        generation
                    })
                });
                let Ok(generation) = generation else {
                    break;
                };
                let service = transfers.clone();
                let result = executor
                    .run(move |cancellation| async move {
                        let rows = service.list().await;
                        let recovery = service.list_recovery(cancellation).await;
                        Ok((rows, recovery))
                    })
                    .await;
                cx.update(|app| {
                    let _ = this.update(app, |pane, cx| {
                        if !pane.queue.accepts_transfer_refresh(generation) {
                            return;
                        }
                        match result {
                            Ok((rows, recovery)) => {
                                pane.apply_transfer_rows(rows, cx);
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

    /// 取消在目标项回执处延后、在项间停止后续提交；最终回执屏障确定迟到取消竞态。
    ///
    /// @author kongweiguang
    fn cancel_transfer(
        &mut self,
        id: kerminal_domain::identity::TransferId,
        cx: &mut Context<Self>,
    ) {
        let transfers = self.transfers.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { transfers.cancel(&id).await })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    if let Err(error) = result {
                        pane.status = Some(localized_error(&error));
                    }
                    pane.reload_transfers(cx);
                });
            });
        })
        .detach();
    }

    /// 按原任务 endpoint 决定是否需要同 HostId 的临时 lease，避免把当前 pane 凭据传给无关任务。
    ///
    /// @author kongweiguang
    fn retry_transfer(
        &mut self,
        id: kerminal_domain::identity::TransferId,
        cx: &mut Context<Self>,
    ) {
        let external_target_lease = self.external_target_lease.clone();
        let transfers = self.transfers.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move {
                    let summary = transfers.get(&id).await?;
                    match temporary_host_for_request(&summary.request)? {
                        Some(required_host) => {
                            let Some(lease) = external_target_lease
                                .as_ref()
                                .filter(|lease| lease.host_id() == &required_host)
                            else {
                                return Ok(None);
                            };
                            transfers
                                .retry_with_external_target(&id, lease)
                                .await
                                .map(Some)
                        }
                        None => transfers.retry(&id).await.map(Some),
                    }
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    match result {
                        Ok(Some(summary)) => {
                            pane.status = Some(format!(
                                "{} {}",
                                locale::text("后继任务已建立：", "Retry queued:"),
                                summary.id
                            ))
                        }
                        Ok(None) => {
                            pane.status = Some(locale::text(
                                "原临时目标连接已失效；请重新连接同一目标并重新创建传输任务。旧任务不会切换到新目标。",
                                "The temporary target lease expired. Reconnect to the same target and create a new transfer; this task will not switch targets."
                            ).to_string())
                        }
                        Err(error) => pane.status = Some(localized_error(&error)),
                    }
                    pane.reload_transfers(cx);
                });
            });
        })
        .detach();
    }
}

/// 请求回包允许当前版本首次填充，但不能覆盖已经推送过的新版本。
///
/// @author kongweiguang
fn workspace_projection_response_is_fresh(current: Option<u64>, incoming: u64) -> bool {
    current.is_none_or(|revision| incoming >= revision)
}

/// 请求回执只能填充仍处于发起视图与代次的面板，切页后重开的同一 Entity 也必须拒收旧回执。
///
/// @author kongweiguang
fn workspace_projection_request_is_current(
    workspace_view_active: bool,
    request_generation: u64,
    current_generation: u64,
) -> bool {
    workspace_view_active && request_generation == current_generation
}

/// Root 推送只接受严格更新的版本，重复事件和迟到事件都不触发新一帧。
///
/// @author kongweiguang
fn workspace_projection_push_is_newer(current: Option<u64>, incoming: u64) -> bool {
    current.is_none_or(|revision| incoming > revision)
}

impl BrowserPanel {
    /// 新面板区分已提交清单与空请求，并从无键盘滚动意图开始，避免跨目录继承旧视口目标。
    ///
    /// @author kongweiguang
    fn new(endpoint: FileEndpoint) -> Self {
        let navigation_history = NavigationHistory::new(endpoint.clone());
        Self {
            endpoint,
            requested_endpoint: None,
            listing: None,
            selected: None,
            selection: selection::FileSelection::default(),
            list_focus_handle: None,
            keyboard_focus: FileListKeyboardFocus::default(),
            pending_selection: None,
            pending_file_preview: false,
            path_input: None,
            path_input_endpoint: None,
            _path_subscription: None,
            loading: false,
            generation: 0,
            preview_generation: 0,
            preview_cancellation: None,
            tree: tree::FileTreeBrowser::default(),
            tree_widget: None,
            list_scroll: VirtualListScrollHandle::new(),
            keyboard_scroll_intent: KeyboardScrollIntent::default(),
            sort: FileSort::default(),
            entry_filter: view_mode::FileEntryFilter::default(),
            navigation_history,
        }
    }

    /// 使旧预览响应失效并发出协作取消，导航和选择变化都使用同一代次边界。
    ///
    /// @author kongweiguang
    fn invalidate_preview_request(&mut self) {
        self.preview_generation = self.preview_generation.wrapping_add(1);
        if let Some(cancellation) = self.preview_cancellation.take() {
            cancellation.cancel();
        }
    }
}

impl Drop for BrowserPanel {
    /// pane 被关闭时通知仍在读取的预览协作退出，避免把已关闭视图当作资源 owner。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        self.invalidate_preview_request();
        self.tree.cancel_pending_requests();
    }
}

impl EventEmitter<OpenFileRequested> for FilesPane {}
impl EventEmitter<WorkspaceOverviewRequested> for FilesPane {}
impl EventEmitter<ActivateEditorFileRequested> for FilesPane {}
impl EventEmitter<SftpHostRequested> for FilesPane {}
impl EventEmitter<CreateSftpHostRequested> for FilesPane {}

/// 按稳定错误类别选择英文安全文案，中文仍使用 application 统一的脱敏消息。
///
/// @author kongweiguang
fn localized_error(error: &AppError) -> String {
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

/// 从 endpoint 形成同一文件系统的子项，远端路径仅经领域 join_name 校验。
fn child_endpoint(endpoint: &FileEndpoint, name: &str) -> Option<FileEndpoint> {
    match endpoint {
        FileEndpoint::Local { path } => Some(FileEndpoint::Local {
            path: path.join(name),
        }),
        FileEndpoint::Remote { host_id, path } => Some(FileEndpoint::Remote {
            host_id: host_id.clone(),
            path: path.join_name(name).ok()?,
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
            path: path.join_name(name).ok()?,
        }),
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

/// 获取当前 endpoint 的父目录，同时保留主机、运行时和容器身份。
fn parent_endpoint(endpoint: &FileEndpoint) -> Option<FileEndpoint> {
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

/// 传输完成只刷新当前实际展示或正在请求的精确 endpoint，不打断尚未提交的用户导航。
///
/// @author kongweiguang
fn panel_needs_transfer_refresh(
    committed_endpoint: &FileEndpoint,
    requested_endpoint: Option<&FileEndpoint>,
    loading: bool,
    has_listing: bool,
    refresh_directories: &[FileEndpoint],
) -> bool {
    (has_listing || loading)
        && panel_refresh_target(committed_endpoint, requested_endpoint, loading)
            .is_some_and(|target| refresh_directories.contains(target))
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

/// 仅消费新出现或刚转为 Succeeded 且具有显式目标提交回执的任务；不从请求路径猜测 Rename 结果。
/// 目录任务还可能改变已打开目标目录的内容，所以只在完整 endpoint 精确相等时附加自身目录。
///
/// @author kongweiguang
fn completed_transfer_refresh_directories(
    previous: &[TransferSummary],
    current: &[TransferSummary],
) -> Vec<FileEndpoint> {
    let mut directories = Vec::new();
    for summary in current {
        if summary.status != TransferStatus::Succeeded
            || summary.commit_summary.committed_entries() == 0
            || previous
                .iter()
                .any(|old| old.id == summary.id && old.status == TransferStatus::Succeeded)
        {
            continue;
        }
        let Some(destination) = summary.actual_destination.as_ref() else {
            continue;
        };
        if let Some(parent) = parent_endpoint(destination)
            && !directories.contains(&parent)
        {
            directories.push(parent);
        }
        if summary.request.kind == TransferKind::Directory && !directories.contains(destination) {
            directories.push(destination.clone());
        }
    }
    directories
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

/// 名称必须是一个合法目录项，所有 endpoint adapter 共用该 UI 前置校验。
fn valid_item_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
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
