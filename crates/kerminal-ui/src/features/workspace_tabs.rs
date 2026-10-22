//! 工作区标签与标签组的纯投影视图，只向 Root 发布用户交互事件。
//!
//! @author kongweiguang

use std::{cell::Cell, collections::HashMap, rc::Rc};

use gpui_kit::{
    AppContext as _, Bounds, Context, Entity, EventEmitter, Pixels, Point, ScrollHandle,
    SharedString, Subscription, Task, Window,
    component::{
        WindowExt as _,
        input::{InputEvent, InputState},
    },
    px,
};
use kerminal_application::workspace::{WorkspaceTabGroupColor, WorkspaceTargetRef};
use kerminal_domain::{identity::TabId, workspace::TerminalLayoutNode};

use crate::{features::connections::HostNavigation, theme::ThemeTokens};

/// Root 提供稳定 Tab 身份、名称、分组、选中态与布局树；组件不持有会话资源。
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceTabProjection {
    pub id: TabId,
    pub title: SharedString,
    pub group_id: Option<String>,
    pub active: bool,
    pub kind: WorkspaceTabKind,
    pub layout: Option<TerminalLayoutNode>,
}

/// 标签组只读投影让空组也有可见且可拖放的目标，不在 UI 维护第二份领域状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceTabGroupProjection {
    pub id: String,
    pub title: SharedString,
    pub color: Option<WorkspaceTabGroupColor>,
    pub collapsed: bool,
    pub order: usize,
}

/// 组件仅展示已知 tab 类型；未来类型保留名称而不伪装成终端或文件标签。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceTabKind {
    Terminal,
    SftpTransfer,
    SftpWorkbench,
    WorkspaceFile,
    Unsupported(SharedString),
}

/// 自动滚动任务绑定真实拖拽源；源失效、拖动终止或窗口失焦时即可停止唯一计时循环。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
enum WorkspaceTabDragSource {
    Tab(TabId),
    Group(String),
}

/// Tab 拖放相对目标项的插入方向；由视图按目标边界计算，controller 再按稳定 ID 校验提交。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabDropPlacement {
    Before,
    After,
}

/// 文件标签菜单只发出瞬时 UI 意图，操作目标始终是菜单打开时捕获的稳定 TabId。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceFileTabAction {
    CopyFullPath,
    Reload,
}

/// 标签组件只描述用户意图；Root 验证并通过 WorkspaceService 修改真实工作区。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceTabEvent {
    FileAction {
        tab_id: TabId,
        action: WorkspaceFileTabAction,
    },
    Activate(TabId),
    Close(TabId),
    CloseBatch(WorkspaceCloseBatchIntent),
    Reorder {
        tab_id: TabId,
        target_index: usize,
    },
    DropOnTab {
        tab_id: TabId,
        target_tab_id: TabId,
        placement: TabDropPlacement,
    },
    MoveToUngroupedEnd(TabId),
    MoveWithinGroup {
        tab_id: TabId,
        group_id: String,
        direction: GroupTabMoveDirection,
    },
    MoveToGroup {
        tab_id: TabId,
        group_id: Option<String>,
    },
    CreateGroupForTab {
        tab_id: TabId,
        title: String,
    },
    RenameTab {
        tab_id: TabId,
        title: String,
    },
    RenameGroup {
        group_id: String,
        title: String,
    },
    SetGroupColor {
        group_id: String,
        color: Option<WorkspaceTabGroupColor>,
    },
    SetGroupCollapsed {
        group_id: String,
        collapsed: bool,
    },
    MoveGroup {
        group_id: String,
        before_group_id: Option<String>,
    },
    Ungroup(String),
}

/// 单标签确认在弹层打开前捕获原焦点，随后只确认或撤销同一请求票据。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceTabCloseConfirmationFocusPhase {
    Requested,
    Confirmed,
    Cancelled,
}

/// Close confirmation binds focus capture to either one tab or the batch's frozen target list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceTabCloseConfirmationFocusTarget {
    Tab(TabId),
    Batch(WorkspaceCloseBatchIntent),
}

/// 同步回调把确认生命周期交给 Root，避免依赖 GPUI 事件在弹层打开前完成派发；可变 Window 用于真实 dialog 门禁。
pub(crate) type WorkspaceTabCloseConfirmationFocusHook = Rc<
    dyn Fn(
        WorkspaceTabCloseConfirmationFocusPhase,
        &WorkspaceTabCloseConfirmationFocusTarget,
        u64,
        &mut Window,
        &mut Context<WorkspaceTabs>,
    ),
>;

/// 共享 SFTP 工作台只属于当前窗口视图；激活时它必须独占选中态，不能连带改写持久快照。
///
/// @author kongweiguang
pub(crate) fn append_shared_sftp_workbench_projection(
    projections: &mut Vec<WorkspaceTabProjection>,
    tab_id: &TabId,
    active: bool,
) -> bool {
    if projections.iter().any(|tab| &tab.id == tab_id) {
        return false;
    }
    if active {
        for tab in projections.iter_mut() {
            tab.active = false;
        }
    }
    projections.push(WorkspaceTabProjection {
        id: tab_id.clone(),
        title: crate::locale::text("SFTP 工作台", "SFTP Workbench"),
        group_id: None,
        active,
        kind: WorkspaceTabKind::SftpWorkbench,
        layout: None,
    });
    true
}

/// 批量关闭可以包含临时工作台，但 reducer 只接收持久标签；分组成员和锚点仍必须真实存在。
///
/// @author kongweiguang
pub(crate) fn partition_shared_sftp_workbench_close_batch(
    intent: &WorkspaceCloseBatchIntent,
    workbench_tab_id: &TabId,
) -> Option<(bool, WorkspaceCloseBatchIntent)> {
    let includes_workbench = intent.tab_ids().contains(workbench_tab_id);
    let invalid_anchor = match intent {
        WorkspaceCloseBatchIntent::Group { .. } => false,
        WorkspaceCloseBatchIntent::ToRight { anchor_tab_id, .. } => {
            anchor_tab_id == workbench_tab_id
        }
        WorkspaceCloseBatchIntent::Others { keep_tab_id, .. }
        | WorkspaceCloseBatchIntent::GroupOthers { keep_tab_id, .. } => {
            keep_tab_id == workbench_tab_id
        }
        WorkspaceCloseBatchIntent::OutsideGroup { keep_tab_ids, .. } => {
            keep_tab_ids.contains(workbench_tab_id)
        }
    };
    if invalid_anchor
        || (includes_workbench
            && matches!(
                intent,
                WorkspaceCloseBatchIntent::Group { .. }
                    | WorkspaceCloseBatchIntent::GroupOthers { .. }
            ))
    {
        return None;
    }
    if !includes_workbench {
        return Some((false, intent.clone()));
    }

    let remaining = intent
        .tab_ids()
        .iter()
        .filter(|tab_id| *tab_id != workbench_tab_id)
        .cloned()
        .collect::<Vec<_>>();
    let filtered = match intent {
        WorkspaceCloseBatchIntent::Group { group_id, .. } => WorkspaceCloseBatchIntent::Group {
            group_id: group_id.clone(),
            tab_ids: remaining,
        },
        WorkspaceCloseBatchIntent::ToRight { anchor_tab_id, .. } => {
            WorkspaceCloseBatchIntent::ToRight {
                anchor_tab_id: anchor_tab_id.clone(),
                tab_ids: remaining,
            }
        }
        WorkspaceCloseBatchIntent::Others { keep_tab_id, .. } => {
            WorkspaceCloseBatchIntent::Others {
                keep_tab_id: keep_tab_id.clone(),
                tab_ids: remaining,
            }
        }
        WorkspaceCloseBatchIntent::GroupOthers {
            group_id,
            keep_tab_id,
            ..
        } => WorkspaceCloseBatchIntent::GroupOthers {
            group_id: group_id.clone(),
            keep_tab_id: keep_tab_id.clone(),
            tab_ids: remaining,
        },
        WorkspaceCloseBatchIntent::OutsideGroup {
            group_id,
            keep_tab_ids,
            ..
        } => WorkspaceCloseBatchIntent::OutsideGroup {
            group_id: group_id.clone(),
            keep_tab_ids: keep_tab_ids.clone(),
            tab_ids: remaining,
        },
    };
    Some((true, filtered))
}

/// 标识需要由 Root 临时视图处理的事件目标，避免 reducer 对 synthetic Tab 伪报成功或无关失败。
///
/// @author kongweiguang
pub(crate) fn workspace_tab_event_targets(event: &WorkspaceTabEvent, tab_id: &TabId) -> bool {
    match event {
        WorkspaceTabEvent::FileAction { tab_id: target, .. }
        | WorkspaceTabEvent::Activate(target)
        | WorkspaceTabEvent::Close(target)
        | WorkspaceTabEvent::Reorder { tab_id: target, .. }
        | WorkspaceTabEvent::MoveWithinGroup { tab_id: target, .. }
        | WorkspaceTabEvent::MoveToGroup { tab_id: target, .. }
        | WorkspaceTabEvent::MoveToUngroupedEnd(target)
        | WorkspaceTabEvent::CreateGroupForTab { tab_id: target, .. }
        | WorkspaceTabEvent::RenameTab { tab_id: target, .. } => target == tab_id,
        WorkspaceTabEvent::DropOnTab {
            tab_id: source,
            target_tab_id,
            ..
        } => source == tab_id || target_tab_id == tab_id,
        WorkspaceTabEvent::CloseBatch(intent) => {
            intent.tab_ids().contains(tab_id)
                || match intent {
                    WorkspaceCloseBatchIntent::ToRight { anchor_tab_id, .. } => {
                        anchor_tab_id == tab_id
                    }
                    WorkspaceCloseBatchIntent::Others { keep_tab_id, .. }
                    | WorkspaceCloseBatchIntent::GroupOthers { keep_tab_id, .. } => {
                        keep_tab_id == tab_id
                    }
                    WorkspaceCloseBatchIntent::OutsideGroup { keep_tab_ids, .. } => {
                        keep_tab_ids.contains(tab_id)
                    }
                    WorkspaceCloseBatchIntent::Group { .. } => false,
                }
        }
        WorkspaceTabEvent::RenameGroup { .. }
        | WorkspaceTabEvent::SetGroupColor { .. }
        | WorkspaceTabEvent::SetGroupCollapsed { .. }
        | WorkspaceTabEvent::MoveGroup { .. }
        | WorkspaceTabEvent::Ungroup(_) => false,
    }
}

/// 新建终端意图单独于工作区 reducer 事件，Root 可先复核导航快照再准入资源创建。
///
/// @author kongweiguang
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceTabsCreateEvent {
    Local,
    Target(WorkspaceTargetRef),
}

/// 只为文件标签生成路径操作事件，避免终端或未来未知标签伪装成文件目标。
///
/// @author kongweiguang
pub(super) fn workspace_file_context_action_events(
    kind: &WorkspaceTabKind,
    tab_id: &TabId,
) -> Vec<WorkspaceTabEvent> {
    if !matches!(kind, WorkspaceTabKind::WorkspaceFile) {
        return Vec::new();
    }

    [
        WorkspaceFileTabAction::CopyFullPath,
        WorkspaceFileTabAction::Reload,
    ]
    .into_iter()
    .map(|action| WorkspaceTabEvent::FileAction {
        tab_id: tab_id.clone(),
        action,
    })
    .collect()
}

/// 组内菜单只表达相邻移动方向，不携带会因拖动或异步刷新而过期的数组索引。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupTabMoveDirection {
    Earlier,
    Later,
}

/// 批量关闭携带用户点击时的目标快照，异步确认和重试都不会扩大范围。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceCloseBatchIntent {
    Group {
        group_id: String,
        tab_ids: Vec<TabId>,
    },
    ToRight {
        anchor_tab_id: TabId,
        tab_ids: Vec<TabId>,
    },
    Others {
        keep_tab_id: TabId,
        tab_ids: Vec<TabId>,
    },
    GroupOthers {
        group_id: String,
        keep_tab_id: TabId,
        tab_ids: Vec<TabId>,
    },
    OutsideGroup {
        group_id: String,
        keep_tab_ids: Vec<TabId>,
        tab_ids: Vec<TabId>,
    },
}

impl WorkspaceCloseBatchIntent {
    /// 返回事件携带的原始目标快照，controller 仅按这些 ID 逐项执行关闭。
    ///
    /// @author kongweiguang
    pub fn tab_ids(&self) -> &[TabId] {
        match self {
            Self::Group { tab_ids, .. }
            | Self::ToRight { tab_ids, .. }
            | Self::Others { tab_ids, .. }
            | Self::GroupOthers { tab_ids, .. }
            | Self::OutsideGroup { tab_ids, .. } => tab_ids,
        }
    }
}

/// 标签视图只保存 Root 的只读投影和临时表单，不复制 active tab 或持久化业务状态。
pub struct WorkspaceTabs {
    projection: Vec<WorkspaceTabProjection>,
    groups: Vec<WorkspaceTabGroupProjection>,
    host_navigation: Option<Entity<HostNavigation>>,
    create_target_picker_open: bool,
    create_target_query: String,
    create_target_search_input: Option<Entity<InputState>>,
    create_target_search_subscription: Option<Subscription>,
    show_tab_numbers: bool,
    name_input: Option<Entity<InputState>>,
    name_subscription: Option<Subscription>,
    name_key_subscription: Option<Subscription>,
    name_key_snapshot: Rc<Cell<Option<WorkspaceNameEditKeySnapshot>>>,
    name_error: Option<SharedString>,
    name_edit_target: Option<WorkspaceNameEditTarget>,
    name_edit_generation: u64,
    confirm_terminal_close: bool,
    close_confirmation_open: bool,
    close_confirmation_generation: u64,
    close_confirmation_focus_hook: Option<WorkspaceTabCloseConfirmationFocusHook>,
    /// 稳定焦点句柄与 TabId 一一对应，重绘和重排不会把键盘焦点转移给另一会话。
    tab_focus_handles: HashMap<TabId, gpui_kit::FocusHandle>,
    /// 键盘导航焦点独立于活动 Tab；仅影响 roving tab stop，不发布业务切换事件。
    roving_focus_tab_id: Option<TabId>,
    /// 用于让悬停标签两侧的分隔线与当前面板底色衔接。
    hovered_tab_id: Option<TabId>,
    /// 同一拖动只保留一个边缘滚动任务，换源/取消时丢弃句柄并增加代次。
    drag_scroll_task: Option<Task<()>>,
    drag_scroll_generation: u64,
    drag_scroll_source: Option<WorkspaceTabDragSource>,
    scroll_handle: ScrollHandle,
    show_tab_picker: bool,
    tab_reveal_generation: u64,
    pending_tab_reveal: Option<PendingTabReveal>,
}

/// 将活动项或键盘焦点的显式可见请求绑定稳定 TabId、代次与起始偏移。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TabRevealReason {
    ActiveSelection,
    KeyboardFocus,
}

/// 将显式可见请求绑定目标、原因与起始偏移，迟到回调不能覆盖新选择或用户滚动。
#[derive(Clone)]
struct PendingTabReveal {
    tab_id: TabId,
    generation: u64,
    expected_offset: Point<Pixels>,
    reason: TabRevealReason,
}

/// 共用单一名称草稿编辑组和标签，避免切换操作时留下多个不可见输入实体。
#[derive(Clone, Debug, Eq, PartialEq)]
enum WorkspaceNameEditTarget {
    CreateGroup { tab_id: TabId },
    RenameTab { tab_id: TabId },
    RenameGroup { group_id: String },
}

/// 编辑快捷键只在输入处理前捕获组合状态，避免 IME 先清除标记后误触发表单动作。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkspaceNameEditKeySnapshot {
    generation: u64,
    action: WorkspaceNameEditKeyAction,
    was_composing: bool,
}

/// 单行重命名仅将 Enter/Escape 解释为保存/取消，其余键仍完全交给输入组件。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkspaceNameEditKeyAction {
    Submit,
    Cancel,
}

/// 把 key listener 收到的键名映射成有限编辑动作，不改变输入组件的按键分发。
///
/// @author kongweiguang
fn workspace_name_edit_key_action(key: &str) -> Option<WorkspaceNameEditKeyAction> {
    match key.to_ascii_lowercase().as_str() {
        "enter" => Some(WorkspaceNameEditKeyAction::Submit),
        "escape" => Some(WorkspaceNameEditKeyAction::Cancel),
        _ => None,
    }
}

/// 在全局 action 分发前记录焦点输入的组合状态；interceptor 只观察、不消费按键。
///
/// @author kongweiguang
fn capture_workspace_name_edit_key(
    snapshot: &Cell<Option<WorkspaceNameEditKeySnapshot>>,
    generation: u64,
    key: &str,
    input_is_focused: bool,
    was_composing: bool,
) {
    let Some(action) = workspace_name_edit_key_action(key) else {
        snapshot.set(None);
        return;
    };
    snapshot.set(input_is_focused.then_some(WorkspaceNameEditKeySnapshot {
        generation,
        action,
        was_composing,
    }));
}

/// 仅让同一编辑代次中、非组合态按下的 Enter/Escape 结束名称编辑。
///
/// @author kongweiguang
fn take_workspace_name_edit_key_action(
    snapshot: &Cell<Option<WorkspaceNameEditKeySnapshot>>,
    generation: u64,
    key: &str,
) -> Option<WorkspaceNameEditKeyAction> {
    let captured = snapshot.take()?;
    let action = workspace_name_edit_key_action(key)?;
    (captured.generation == generation && captured.action == action && !captured.was_composing)
        .then_some(action)
}

/// 将编辑目标与已验证名称绑定成原始操作意图，避免编辑期间投影刷新时改到另一标签。
///
/// @author kongweiguang
fn workspace_name_edit_event(target: WorkspaceNameEditTarget, title: String) -> WorkspaceTabEvent {
    match target {
        WorkspaceNameEditTarget::CreateGroup { tab_id } => {
            WorkspaceTabEvent::CreateGroupForTab { tab_id, title }
        }
        WorkspaceNameEditTarget::RenameTab { tab_id } => {
            WorkspaceTabEvent::RenameTab { tab_id, title }
        }
        WorkspaceNameEditTarget::RenameGroup { group_id } => {
            WorkspaceTabEvent::RenameGroup { group_id, title }
        }
    }
}

/// 将拖放两端都绑定到捕获时的稳定身份，避免投影重排后用旧数组索引猜目标。
///
/// @author kongweiguang
fn tab_drop_event(
    tab_id: TabId,
    target_tab_id: TabId,
    placement: TabDropPlacement,
) -> WorkspaceTabEvent {
    WorkspaceTabEvent::DropOnTab {
        tab_id,
        target_tab_id,
        placement,
    }
}

/// 将空白标签栏上的类型化标签拖放收敛为移出分组意图，不改变标签或会话身份。
///
/// @author kongweiguang
pub(super) fn ungrouped_tab_drop_event(tab_id: TabId) -> WorkspaceTabEvent {
    WorkspaceTabEvent::MoveToGroup {
        tab_id,
        group_id: None,
    }
}

/// 末尾空白只携带源身份，避免把合成工作台标签误当持久化落点或拆成两次保存。
/// 过期投影静默忽略；只携带源身份，避免临时工作台成为不可持久化的落点。
///
/// @author kongweiguang
pub(super) fn trailing_tab_drop_event(
    tab_id: &TabId,
    projection: &[WorkspaceTabProjection],
) -> Option<WorkspaceTabEvent> {
    if !projection.iter().any(|tab| &tab.id == tab_id) {
        return None;
    }
    if projection
        .last()
        .is_some_and(|last| &last.id == tab_id && last.group_id.is_none())
    {
        return None;
    }
    Some(WorkspaceTabEvent::MoveToUngroupedEnd(tab_id.clone()))
}

/// 将指针映射到目标标签的前后半区，左右边界都由该帧实测 bounds 决定。
///
/// @author kongweiguang
pub(super) fn tab_drop_placement(
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
) -> TabDropPlacement {
    if position.x < bounds.left() + bounds.size.width / 2. {
        TabDropPlacement::Before
    } else {
        TabDropPlacement::After
    }
}

/// 把组头的左右落点翻译为原有的 before-group 契约，末组右半区明确表示追加。
/// 外层 None 表示 target 已失效，内层 None 表示合法追加，避免二者被混淆。
///
/// @author kongweiguang
pub(super) fn group_drop_before_id(
    ordered_group_ids: &[String],
    target_group_id: &str,
    placement: TabDropPlacement,
) -> Option<Option<String>> {
    let index = ordered_group_ids
        .iter()
        .position(|group_id| group_id == target_group_id)?;
    Some(match placement {
        TabDropPlacement::Before => Some(target_group_id.to_owned()),
        TabDropPlacement::After => ordered_group_ids.get(index + 1).cloned(),
    })
}

/// 键盘方向键只移动焦点，Enter/Space 仍由标签事件单独激活当前目标。
///
/// @author kongweiguang
pub(super) fn workspace_tab_keyboard_target_index(
    ordered_tab_ids: &[TabId],
    current_tab_id: &TabId,
    key: &str,
) -> Option<usize> {
    if ordered_tab_ids.is_empty() {
        return None;
    }
    let current = ordered_tab_ids
        .iter()
        .position(|tab_id| tab_id == current_tab_id)?;
    match key.to_ascii_lowercase().as_str() {
        "left" | "arrowleft" => Some(if current == 0 {
            ordered_tab_ids.len() - 1
        } else {
            current - 1
        }),
        "right" | "arrowright" => Some((current + 1) % ordered_tab_ids.len()),
        "home" => Some(0),
        "end" => Some(ordered_tab_ids.len() - 1),
        _ => None,
    }
}

/// 在滚动视口两侧按指针距离分级推进偏移，并夹在 GPUI 的有效负向滚动范围内。
/// 每个计时步只推进有限距离；指针离开视口、拖动结束或窗口失焦都会停止推进。
///
/// @author kongweiguang
pub(super) fn next_tab_drag_scroll_offset(
    pointer: Point<Pixels>,
    viewport: Bounds<Pixels>,
    current_offset: Point<Pixels>,
    max_offset: Point<Pixels>,
) -> Option<Point<Pixels>> {
    const EDGE_ZONE: f32 = 28.;
    const MAX_STEP: f32 = 8.;

    if !viewport.contains(&pointer) {
        return None;
    }
    let pointer_x = pointer.x;
    let distance_from_left = (pointer_x - viewport.left()).as_f32();
    let distance_from_right = (viewport.right() - pointer_x).as_f32();
    let mut next = current_offset;
    if (0.0..EDGE_ZONE).contains(&distance_from_left) {
        let proximity = 1.0 - distance_from_left / EDGE_ZONE;
        next.x += px(MAX_STEP * proximity);
    } else if (0.0..EDGE_ZONE).contains(&distance_from_right) {
        let proximity = 1.0 - distance_from_right / EDGE_ZONE;
        next.x -= px(MAX_STEP * proximity);
    } else {
        return None;
    }

    next.x = next.x.clamp(-max_offset.x, px(0.));
    (next != current_offset).then_some(next)
}

/// 以实测标签与视口 bounds 计算刚好露出 active tab 的水平偏移，不猜组宽、不滚到末尾。
/// 每个标签的 bounds 由所在行的 GPUI prepaint 回调提供，避免 ScrollHandle 只测到外层 wrapper。
///
/// @author kongweiguang
fn minimal_active_tab_reveal_offset(
    current_offset: Point<Pixels>,
    viewport: Bounds<Pixels>,
    tab: Bounds<Pixels>,
) -> Point<Pixels> {
    let delta = if tab.size.width > viewport.size.width || tab.left() < viewport.left() {
        viewport.left() - tab.left()
    } else if tab.right() > viewport.right() {
        viewport.right() - tab.right()
    } else {
        px(0.)
    };
    gpui_kit::point(current_offset.x + delta, current_offset.y)
}

/// 拓扑或活动身份变化使旧 prepaint 测量失效；只有 active TabId 改变才创建活动项 reveal。
/// 同一拓扑的标题等元数据更新会递增代次并保留有效请求，让新帧重新测量且旧回调失效。
///
/// @author kongweiguang
fn reconcile_active_tab_reveal_projection(
    current_projection: &[WorkspaceTabProjection],
    next_projection: &[WorkspaceTabProjection],
    generation: &mut u64,
    pending: &mut Option<PendingTabReveal>,
    expected_offset: Point<Pixels>,
) {
    if current_projection == next_projection {
        return;
    }

    let same_tab_topology = current_projection.len() == next_projection.len()
        && current_projection
            .iter()
            .zip(next_projection)
            .all(|(current, next)| {
                current.id == next.id
                    && current.group_id == next.group_id
                    && current.active == next.active
            });
    if same_tab_topology {
        if let Some(pending) = pending.as_mut() {
            *generation = (*generation).wrapping_add(1);
            pending.generation = *generation;
        }
        return;
    }

    let previous_active = current_projection
        .iter()
        .find(|tab| tab.active)
        .map(|tab| &tab.id);
    let next_active = next_projection
        .iter()
        .find(|tab| tab.active)
        .map(|tab| &tab.id);

    invalidate_tab_reveal(generation, pending);
    if previous_active != next_active {
        *pending = next_active.map(|tab_id| PendingTabReveal {
            tab_id: tab_id.clone(),
            generation: *generation,
            expected_offset,
            reason: TabRevealReason::ActiveSelection,
        });
    }
}

/// 分组拓扑变化会移动或隐藏标签行，所以清除旧测量且不新增自动滚动请求。
/// 调用方应先更新组投影再更新标签投影，使同一批次中的 active 变化最后建立新请求。
///
/// @author kongweiguang
fn invalidate_tab_reveal_for_group_change(
    current_groups: &[WorkspaceTabGroupProjection],
    next_groups: &[WorkspaceTabGroupProjection],
    generation: &mut u64,
    pending: &mut Option<PendingTabReveal>,
) {
    if current_groups != next_groups {
        invalidate_tab_reveal(generation, pending);
    }
}

/// 递增测量代次并丢弃请求；迟到 callback 必须同时匹配稳定 TabId 与此代次。
///
/// @author kongweiguang
fn invalidate_tab_reveal(generation: &mut u64, pending: &mut Option<PendingTabReveal>) {
    *generation = (*generation).wrapping_add(1);
    *pending = None;
}

impl EventEmitter<WorkspaceTabEvent> for WorkspaceTabs {}

impl EventEmitter<WorkspaceTabsCreateEvent> for WorkspaceTabs {}

mod render;

impl WorkspaceTabs {
    /// 初始视图不依赖窗口构建输入控件，避免 Root 的现有 constructor 需要第二种装配路径。
    ///
    /// @author kongweiguang
    pub fn new(projection: Vec<WorkspaceTabProjection>) -> Self {
        Self {
            projection,
            groups: Vec::new(),
            host_navigation: None,
            create_target_picker_open: false,
            create_target_query: String::new(),
            create_target_search_input: None,
            create_target_search_subscription: None,
            show_tab_numbers: false,
            name_input: None,
            name_subscription: None,
            name_key_subscription: None,
            name_key_snapshot: Rc::new(Cell::new(None)),
            name_error: None,
            name_edit_target: None,
            name_edit_generation: 0,
            confirm_terminal_close: true,
            close_confirmation_open: false,
            close_confirmation_generation: 0,
            close_confirmation_focus_hook: None,
            tab_focus_handles: HashMap::new(),
            roving_focus_tab_id: None,
            hovered_tab_id: None,
            drag_scroll_task: None,
            drag_scroll_generation: 0,
            drag_scroll_source: None,
            scroll_handle: ScrollHandle::new(),
            show_tab_picker: false,
            tab_reveal_generation: 0,
            pending_tab_reveal: None,
        }
    }

    /// 标签投影来自 controller；真实拓扑变化先使旧测量失效，只有 active ID 改变才请求一次 reveal。
    ///
    /// @author kongweiguang
    pub fn set_projection(
        &mut self,
        projection: Vec<WorkspaceTabProjection>,
        cx: &mut Context<Self>,
    ) {
        reconcile_active_tab_reveal_projection(
            &self.projection,
            &projection,
            &mut self.tab_reveal_generation,
            &mut self.pending_tab_reveal,
            self.scroll_handle.offset(),
        );
        self.projection = projection;
        cx.notify();
    }

    /// 用 controller 的完整组投影刷新空组和组标题；实际组变化会让旧标签 bounds 测量失效。
    ///
    /// @author kongweiguang
    pub fn set_groups(&mut self, groups: Vec<WorkspaceTabGroupProjection>, cx: &mut Context<Self>) {
        invalidate_tab_reveal_for_group_change(
            &self.groups,
            &groups,
            &mut self.tab_reveal_generation,
            &mut self.pending_tab_reveal,
        );
        self.groups = groups;
        cx.notify();
    }

    /// 仅在用户明确用键盘移动焦点时请求同一套真实边界测量，普通重绘不改用户滚动位置。
    ///
    /// @author kongweiguang
    fn request_keyboard_tab_reveal(&mut self, tab_id: TabId) {
        invalidate_tab_reveal(
            &mut self.tab_reveal_generation,
            &mut self.pending_tab_reveal,
        );
        self.pending_tab_reveal = Some(PendingTabReveal {
            tab_id,
            generation: self.tab_reveal_generation,
            expected_offset: self.scroll_handle.offset(),
            reason: TabRevealReason::KeyboardFocus,
        });
    }

    /// 创建菜单在打开时读取同一个主机导航快照，避免 UI 复制主机目录或使用过期目标。
    ///
    /// @author kongweiguang
    pub fn set_host_navigation(
        &mut self,
        host_navigation: Option<Entity<HostNavigation>>,
        cx: &mut Context<Self>,
    ) {
        self.host_navigation = host_navigation;
        cx.notify();
    }

    /// 延迟创建一份持久搜索输入，避免标签栏重绘时丢失焦点、IME 组合态或筛选内容。
    ///
    /// @author kongweiguang
    pub(super) fn ensure_create_target_search_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(input) = self.create_target_search_input.as_ref() {
            return input.clone();
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text(
                "搜索名称、地址、协议或分组",
                "Search names, addresses, protocols, or groups",
            ))
        });
        self.create_target_search_subscription =
            Some(cx.subscribe(&input, |tabs, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    tabs.create_target_query = input.read(cx).value().to_string();
                    cx.notify();
                }
            }));
        self.create_target_search_input = Some(input.clone());
        input
    }

    /// 右键 popover 的受控状态由标签栏 owner 管理；关闭清空旧查询，重开时始终从完整目录开始。
    ///
    /// @author kongweiguang
    pub(super) fn set_create_target_picker_open(
        &mut self,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.create_target_picker_open == open {
            return;
        }
        self.create_target_picker_open = open;
        self.create_target_query.clear();
        let input = if open {
            Some(self.ensure_create_target_search_input(window, cx))
        } else {
            self.create_target_search_input.clone()
        };
        if let Some(input) = input {
            input.update(cx, |input, cx| {
                input.set_value(String::new(), window, cx);
                if open {
                    input.focus(window, cx);
                }
            });
        }
        cx.notify();
    }

    /// Root 需要识别此非模态输入面板，避免全局快捷键把搜索键送到背景终端。
    ///
    /// @author kongweiguang
    pub(crate) fn is_create_target_picker_open(&self) -> bool {
        self.create_target_picker_open
    }

    /// 关闭偏好来自 Root 已加载的设置快照；视图本身不再读取配置仓储。
    pub fn set_confirm_terminal_close(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.confirm_terminal_close != enabled {
            self.confirm_terminal_close = enabled;
            cx.notify();
        }
    }

    /// Root 提供同步确认回调；弹层打开前捕获，确认/取消只更新同一请求的状态。
    ///
    /// @author kongweiguang
    pub(crate) fn set_close_confirmation_focus_hook(
        &mut self,
        hook: Option<WorkspaceTabCloseConfirmationFocusHook>,
        cx: &mut Context<Self>,
    ) {
        self.close_confirmation_focus_hook = hook;
        cx.notify();
    }

    /// 主题设置只控制展示，不改变 tab 顺序、快捷键或持久化快照。
    pub fn set_show_tab_numbers(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.show_tab_numbers != enabled {
            self.show_tab_numbers = enabled;
            cx.notify();
        }
    }

    /// 依照 controller 的全局排序生成组区域，并为每个标签保留原始索引用于重排。
    fn grouped_indices(&self) -> Vec<(Option<String>, Vec<usize>)> {
        let mut ordered_groups = self.groups.iter().collect::<Vec<_>>();
        ordered_groups.sort_by_key(|group| group.order);
        let mut groups = ordered_groups
            .into_iter()
            .map(|group| (Some(group.id.clone()), Vec::new()))
            .collect::<Vec<_>>();
        groups.push((None, Vec::new()));

        for (index, tab) in self.projection.iter().enumerate() {
            let group_index = match groups
                .iter()
                .position(|(group_id, _)| group_id.as_ref() == tab.group_id.as_ref())
            {
                Some(group_index) => group_index,
                None => {
                    groups.push((tab.group_id.clone(), Vec::new()));
                    groups.len() - 1
                }
            };
            groups[group_index].1.push(index);
        }
        groups
    }

    /// 将显示序号附加在排序投影上，避免按分组或可见区域重新编号造成快捷操作歧义。
    fn display_title(&self, index: usize) -> SharedString {
        let title = &self.projection[index].title;
        if self.show_tab_numbers {
            SharedString::from(format!("{}  {}", index + 1, title))
        } else {
            title.clone()
        }
    }

    /// 右端列表沿用组内显示顺序，但不按折叠或滚动位置过滤，隐藏标签始终有可达入口。
    /// 菜单保存完整标题和稳定 TabId，而不是索引；打开后重排也不会激活另一标签。
    ///
    /// @author kongweiguang
    fn tab_picker_entries(&self) -> Vec<(TabId, SharedString, bool)> {
        self.grouped_indices()
            .into_iter()
            .flat_map(|(_, indices)| indices)
            .map(|index| {
                let tab = &self.projection[index];
                (tab.id.clone(), self.display_title(index), tab.active)
            })
            .collect()
    }

    /// 编辑目标从当前稳定投影捕获，后续保存由 controller 验证 ID 是否仍有效。
    fn begin_tab_rename(&mut self, tab_id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(title) = self
            .projection
            .iter()
            .find(|tab| tab.id == tab_id)
            .map(|tab| tab.title.to_string())
        else {
            self.name_error = Some(crate::locale::text(
                "标签已关闭，无法重命名。",
                "The tab is no longer available to rename.",
            ));
            cx.notify();
            return;
        };
        self.begin_name_edit(
            WorkspaceNameEditTarget::RenameTab { tab_id },
            title,
            crate::locale::text("标签名称", "Tab name"),
            window,
            cx,
        );
    }

    /// 组改名单独使用已选组 ID，编辑中投影变化时仍只提交原目标。
    fn begin_group_rename(
        &mut self,
        group_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(title) = self
            .groups
            .iter()
            .find(|group| group.id == group_id)
            .map(|group| group.title.to_string())
        else {
            self.name_error = Some(crate::locale::text(
                "分组已不存在，无法重命名。",
                "The group no longer exists to rename.",
            ));
            cx.notify();
            return;
        };
        self.begin_name_edit(
            WorkspaceNameEditTarget::RenameGroup { group_id },
            title,
            crate::locale::text("分组名称", "Group name"),
            window,
            cx,
        );
    }

    /// 名称输入固定替换 36px 标签行，避免在标题栏裁剪区下方追加不可见表单；键盘观察只绑定本次聚焦输入与编辑代次。
    ///
    /// @author kongweiguang
    fn begin_name_edit(
        &mut self,
        target: WorkspaceNameEditTarget,
        initial: String,
        placeholder: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.name_edit_generation = self.name_edit_generation.wrapping_add(1);
        let generation = self.name_edit_generation;
        self.name_subscription = None;
        self.name_key_subscription = None;
        self.name_key_snapshot.set(None);
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(initial)
                .placeholder(placeholder)
        });
        self.name_input = Some(input.clone());
        self.name_subscription = Some(cx.subscribe(&input, |tabs, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) && tabs.name_error.take().is_some() {
                cx.notify();
            }
        }));
        self.name_edit_target = Some(target);
        self.name_error = None;

        let input_focus_handle = input.read(cx).presentation().focus_handle().clone();
        let input_for_key_snapshot = input.clone();
        let tabs_for_key_snapshot = cx.weak_entity();
        let key_snapshot = self.name_key_snapshot.clone();
        self.name_key_subscription = Some(cx.intercept_keystrokes(move |event, window, cx| {
            let focused = input_focus_handle.is_focused(window)
                && tabs_for_key_snapshot.upgrade().is_some_and(|tabs| {
                    let tabs = tabs.read(cx);
                    tabs.name_edit_generation == generation && tabs.name_edit_target.is_some()
                });
            let was_composing = focused && input_for_key_snapshot.read(cx).is_composing();
            capture_workspace_name_edit_key(
                &key_snapshot,
                generation,
                &event.keystroke.key,
                focused,
                was_composing,
            );
        }));
        cx.notify();

        let tabs_entity = cx.entity().clone();
        window.on_next_frame(move |window, app| {
            tabs_entity.update(app, |tabs, cx| {
                if tabs.name_edit_generation != generation || tabs.name_edit_target.is_none() {
                    return;
                }
                input.update(cx, |state, cx| state.focus(window, cx));
            });
        });
    }

    /// 名称校验在发事件前完成；失败时保留焦点和草稿，实际组唯一性和过期 ID 仍由 controller 再验证。
    ///
    /// @author kongweiguang
    fn submit_name_edit(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.name_input.as_ref() else {
            return;
        };
        let title = input.read(cx).value().to_string();
        let Some(target) = self.name_edit_target.clone() else {
            return;
        };
        let validation = match &target {
            WorkspaceNameEditTarget::CreateGroup { .. } => {
                validate_group_title(&title, &self.groups, None)
            }
            WorkspaceNameEditTarget::RenameGroup { group_id } => {
                validate_group_title(&title, &self.groups, Some(group_id))
            }
            WorkspaceNameEditTarget::RenameTab { .. } => validate_workspace_title(&title),
        };
        let title = match validation {
            Ok(title) => title,
            Err(error) => {
                self.name_error = Some(workspace_name_error_text(error));
                cx.notify();
                return;
            }
        };
        let event = workspace_name_edit_event(target, title);
        self.name_input = None;
        self.name_subscription = None;
        self.name_key_subscription = None;
        self.name_key_snapshot.set(None);
        self.name_edit_target = None;
        self.name_error = None;
        self.name_edit_generation = self.name_edit_generation.wrapping_add(1);
        cx.emit(event);
        cx.notify();
    }

    /// 取消只释放名称草稿与快捷键观察，不发工作区事件，也不触碰已有标签组或标签。
    ///
    /// @author kongweiguang
    fn cancel_name_edit(&mut self, cx: &mut Context<Self>) {
        self.name_input = None;
        self.name_subscription = None;
        self.name_key_subscription = None;
        self.name_key_snapshot.set(None);
        self.name_edit_target = None;
        self.name_error = None;
        self.name_edit_generation = self.name_edit_generation.wrapping_add(1);
        cx.notify();
    }

    /// 终端关闭遵循偏好确认；批量焦点票据绑定冻结目标列表，并在 menu dismiss 后、alert 获取焦点前捕获。
    /// 确认由此入口同步关闭弹层，避免 Kit 延迟恢复旧输入焦点覆盖资源 ACK 的后续移交。
    ///
    /// @author kongweiguang
    fn request_close_event(
        &mut self,
        event: WorkspaceTabEvent,
        tab_ids: Vec<TabId>,
        capture_focus_before_dialog: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let focus_target = match &event {
            WorkspaceTabEvent::Close(tab_id) if tab_ids.len() == 1 => Some(
                WorkspaceTabCloseConfirmationFocusTarget::Tab(tab_id.clone()),
            ),
            WorkspaceTabEvent::CloseBatch(intent) => Some(
                WorkspaceTabCloseConfirmationFocusTarget::Batch(intent.clone()),
            ),
            _ => None,
        };
        let terminal_count = tab_ids
            .iter()
            .filter(|tab_id| {
                self.projection.iter().any(|tab| {
                    &tab.id == *tab_id && matches!(&tab.kind, WorkspaceTabKind::Terminal)
                })
            })
            .count();
        if !self.confirm_terminal_close || terminal_count == 0 {
            // A batch still needs its original focus intent when close confirmation is disabled;
            // synthesize the same one-shot acceptance before publishing the event.
            if !self.confirm_terminal_close
                && terminal_count > 0
                && capture_focus_before_dialog
                && let (
                    Some(target @ WorkspaceTabCloseConfirmationFocusTarget::Batch(_)),
                    Some(hook),
                ) = (
                    focus_target.as_ref(),
                    self.close_confirmation_focus_hook.clone(),
                )
            {
                self.close_confirmation_generation =
                    self.close_confirmation_generation.wrapping_add(1);
                let request_id = self.close_confirmation_generation;
                hook(
                    WorkspaceTabCloseConfirmationFocusPhase::Requested,
                    target,
                    request_id,
                    window,
                    cx,
                );
                hook(
                    WorkspaceTabCloseConfirmationFocusPhase::Confirmed,
                    target,
                    request_id,
                    window,
                    cx,
                );
            }
            cx.emit(event);
            return None;
        }
        if self.close_confirmation_open {
            return None;
        }

        self.close_confirmation_open = true;
        self.close_confirmation_generation = self.close_confirmation_generation.wrapping_add(1);
        let request_id = self.close_confirmation_generation;
        let focus_hook = self.close_confirmation_focus_hook.clone();
        if capture_focus_before_dialog
            && let (Some(target), Some(hook)) = (&focus_target, &focus_hook)
        {
            // Run directly before open_alert_dialog moves focus to its confirmation button.
            hook(
                WorkspaceTabCloseConfirmationFocusPhase::Requested,
                target,
                request_id,
                window,
                cx,
            );
        }
        cx.notify();
        let entity = cx.entity().clone();
        let total = tab_ids.len();
        let title = if total == 1 {
            crate::locale::text("关闭终端标签？", "Close terminal tab?")
        } else {
            crate::locale::text("关闭所选标签？", "Close selected tabs?")
        };
        let (zh_description, en_description) =
            self.close_confirmation_description(&tab_ids, terminal_count);
        let description = crate::locale::text(zh_description, en_description);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let confirm_entity = entity.clone();
            let cancel_entity = entity.clone();
            let confirmed_event = event.clone();
            let confirmed_target = focus_target.clone();
            let cancelled_target = focus_target.clone();
            let confirm_focus_hook = focus_hook.clone();
            let cancel_focus_hook = focus_hook.clone();
            alert
                .title(title.clone())
                .description(description.clone())
                .confirm()
                .on_ok(move |_, window, app| {
                    // Close synchronously before publishing the resource operation: Kit otherwise
                    // restores this dialog's old focus after 250ms and can overwrite the ACK handoff.
                    window.close_dialog(app);
                    confirm_entity.update(app, |tabs, cx| {
                        tabs.close_confirmation_open = false;
                        if let (Some(target), Some(hook)) = (&confirmed_target, &confirm_focus_hook)
                        {
                            hook(
                                WorkspaceTabCloseConfirmationFocusPhase::Confirmed,
                                target,
                                request_id,
                                window,
                                cx,
                            );
                        }
                        cx.emit(confirmed_event.clone());
                        cx.notify();
                    });
                    // This callback owns dismissal; returning true would pop a second dialog.
                    false
                })
                .on_cancel(move |_, window, app| {
                    cancel_entity.update(app, |tabs, cx| {
                        tabs.close_confirmation_open = false;
                        if let (Some(target), Some(hook)) = (&cancelled_target, &cancel_focus_hook)
                        {
                            hook(
                                WorkspaceTabCloseConfirmationFocusPhase::Cancelled,
                                target,
                                request_id,
                                window,
                                cx,
                            );
                        }
                        cx.notify();
                    });
                    true
                })
        });
        Some(request_id)
    }

    /// 确认说明按捕获 ID 区分终端、文件与共享工作台；临时工作台只关闭视图，不承诺终止传输任务。
    ///
    /// @author kongweiguang
    fn close_confirmation_description(
        &self,
        tab_ids: &[TabId],
        terminal_count: usize,
    ) -> (String, String) {
        let total = tab_ids.len();
        if total == 1 {
            if self
                .projection
                .iter()
                .any(|tab| tab.id == tab_ids[0] && tab.kind == WorkspaceTabKind::SftpWorkbench)
            {
                return (
                    "将关闭共享 SFTP 工作台视图，已有传输任务会继续运行。".to_owned(),
                    "This closes the shared SFTP workbench view; existing transfers keep running."
                        .to_owned(),
                );
            }
            return (
                "将关闭此终端标签。关闭失败时，标签会保留。".to_owned(),
                "This terminal tab will close. If closing fails, the tab will stay open."
                    .to_owned(),
            );
        }
        if total == terminal_count {
            return (
                format!("将关闭 {terminal_count} 个终端标签。关闭失败的标签会保留。"),
                format!(
                    "This will close {terminal_count} terminal tabs. Tabs that fail to close will stay open."
                ),
            );
        }
        let file_count = tab_ids
            .iter()
            .filter(|tab_id| {
                self.projection.iter().any(|tab| {
                    &tab.id == *tab_id && matches!(&tab.kind, WorkspaceTabKind::WorkspaceFile)
                })
            })
            .count();
        let workbench_count = tab_ids
            .iter()
            .filter(|tab_id| {
                self.projection.iter().any(|tab| {
                    &tab.id == *tab_id && matches!(&tab.kind, WorkspaceTabKind::SftpWorkbench)
                })
            })
            .count();
        let terminal_label = if terminal_count == 1 {
            "terminal"
        } else {
            "terminals"
        };
        if workbench_count > 0 && terminal_count == 0 && file_count == 0 {
            return (
                format!(
                    "将关闭 {total} 个标签，其中 {workbench_count} 个共享 SFTP 工作台视图；已有传输任务会继续运行。"
                ),
                format!(
                    "This will close {total} tabs, including {workbench_count} shared SFTP workbench view(s); existing transfers keep running."
                ),
            );
        }
        if file_count == 0 {
            let workbench_note = if workbench_count > 0 {
                "工作台只会在其它标签全部关闭成功后移除；已有传输任务会继续运行。"
            } else {
                ""
            };
            let workbench_note_en = if workbench_count > 0 {
                " The workbench closes only after the other tabs close successfully; existing transfers keep running."
            } else {
                ""
            };
            return (
                format!(
                    "将关闭 {total} 个标签，其中 {terminal_count} 个终端。终端关闭失败时，标签会保留。{workbench_note}"
                ),
                format!(
                    "This will close {total} tabs, including {terminal_count} {terminal_label}. Terminal tabs that fail to close will stay open.{workbench_note_en}"
                ),
            );
        }
        let file_label = if file_count == 1 { "file" } else { "files" };
        (
            format!(
                "将关闭 {total} 个标签，包含 {terminal_count} 个终端和 {file_count} 个文件。未保存的文件会另行确认。终端关闭失败时，标签会保留。{}",
                if workbench_count > 0 {
                    "工作台只会在其它标签全部关闭成功后移除；已有传输任务会继续运行。"
                } else {
                    ""
                }
            ),
            format!(
                "This will close {total} tabs, including {terminal_count} {terminal_label} and {file_count} {file_label}. You'll be prompted about unsaved files. Terminal tabs that fail to close will stay open.{}",
                if workbench_count > 0 {
                    " The workbench closes only after the other tabs close successfully; existing transfers keep running."
                } else {
                    ""
                }
            ),
        )
    }

    /// UI 菜单路径在打开确认框前同步捕获焦点；Root 持有可变借用时必须调用 prepared-focus 入口，避免 hook 重入。
    ///
    /// @author kongweiguang
    fn request_close_tab(&mut self, tab_id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.request_close_event(
            WorkspaceTabEvent::Close(tab_id.clone()),
            vec![tab_id],
            true,
            window,
            cx,
        );
    }

    /// Root 自身已可变借用时应先捕获焦点，再用返回代次关联弹层，避免同步重入 Root Entity。
    ///
    /// @author kongweiguang
    pub(crate) fn request_close_tab_from_root_with_prepared_focus(
        &mut self,
        tab_id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        self.request_close_event(
            WorkspaceTabEvent::Close(tab_id.clone()),
            vec![tab_id],
            false,
            window,
            cx,
        )
    }

    /// 批量关闭始终使用构造时捕获的目标列表，并在 menu dismiss 后先捕获焦点再打开确认框。
    ///
    /// @author kongweiguang
    fn request_close_batch(
        &mut self,
        intent: WorkspaceCloseBatchIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab_ids = intent.tab_ids().to_vec();
        let _ = self.request_close_event(
            WorkspaceTabEvent::CloseBatch(intent),
            tab_ids,
            true,
            window,
            cx,
        );
    }

    /// 获取当前分组的稳定显示顺序；未分组标签位于末尾且不参与分组重排。
    fn displayed_tab_ids(&self) -> Vec<TabId> {
        self.grouped_indices()
            .into_iter()
            .flat_map(|(_, indices)| {
                indices
                    .into_iter()
                    .map(|index| self.projection[index].id.clone())
            })
            .collect()
    }

    /// 将“关闭组”目标固定为该组当前成员的显示顺序快照。
    ///
    /// @author kongweiguang
    fn close_group_intent(&self, group_id: &str) -> Option<WorkspaceCloseBatchIntent> {
        let indices = self
            .grouped_indices()
            .into_iter()
            .find(|(candidate, _)| candidate.as_deref() == Some(group_id))?
            .1;
        Some(WorkspaceCloseBatchIntent::Group {
            group_id: group_id.to_owned(),
            tab_ids: indices
                .into_iter()
                .map(|index| self.projection[index].id.clone())
                .collect(),
        })
    }

    /// 固定组内保留锚点与其余成员，供既有批量关闭确认和逐项 ACK 流程复用。
    ///
    /// @author kongweiguang
    fn close_group_others_intent(&self, keep_tab_id: &TabId) -> Option<WorkspaceCloseBatchIntent> {
        let group_id = self
            .projection
            .iter()
            .find(|tab| &tab.id == keep_tab_id)?
            .group_id
            .clone()?;
        if !self.groups.iter().any(|group| group.id == group_id) {
            return None;
        }
        Some(WorkspaceCloseBatchIntent::GroupOthers {
            group_id: group_id.clone(),
            keep_tab_id: keep_tab_id.clone(),
            tab_ids: self
                .projection
                .iter()
                .filter(|tab| tab.group_id.as_deref() == Some(group_id.as_str()))
                .map(|tab| tab.id.clone())
                .filter(|tab_id| tab_id != keep_tab_id)
                .collect(),
        })
    }

    /// 捕获完整保留组快照与组外关闭目标，防止确认期间组成员变化扩大或缩小范围。
    ///
    /// @author kongweiguang
    fn close_outside_group_intent(&self, group_id: &str) -> Option<WorkspaceCloseBatchIntent> {
        if !self.groups.iter().any(|group| group.id == group_id) {
            return None;
        }
        let keep_tab_ids = self
            .projection
            .iter()
            .filter(|tab| tab.group_id.as_deref() == Some(group_id))
            .map(|tab| tab.id.clone())
            .collect::<Vec<_>>();
        let displayed = self.displayed_tab_ids();
        Some(WorkspaceCloseBatchIntent::OutsideGroup {
            group_id: group_id.to_owned(),
            tab_ids: displayed
                .into_iter()
                .filter(|tab_id| !keep_tab_ids.contains(tab_id))
                .collect(),
            keep_tab_ids,
        })
    }

    /// 为固定锚点构造后续标签关闭意图，分组折叠状态不会改变其包含的标签集合。
    fn close_right_intent(&self, anchor_tab_id: &TabId) -> Option<WorkspaceCloseBatchIntent> {
        let displayed = self.displayed_tab_ids();
        let anchor_index = displayed
            .iter()
            .position(|tab_id| tab_id == anchor_tab_id)?;
        Some(WorkspaceCloseBatchIntent::ToRight {
            anchor_tab_id: anchor_tab_id.clone(),
            tab_ids: displayed.into_iter().skip(anchor_index + 1).collect(),
        })
    }

    /// “关闭其他标签”排除被右键的锚点，并在操作前固定其余目标列表。
    fn close_others_intent(&self, keep_tab_id: &TabId) -> Option<WorkspaceCloseBatchIntent> {
        let displayed = self.displayed_tab_ids();
        displayed
            .iter()
            .any(|tab_id| tab_id == keep_tab_id)
            .then(|| WorkspaceCloseBatchIntent::Others {
                keep_tab_id: keep_tab_id.clone(),
                tab_ids: displayed
                    .into_iter()
                    .filter(|tab_id| tab_id != keep_tab_id)
                    .collect(),
            })
    }
}

/// 标签组名称校验只约束用户新建的名称，读取旧 v3 组仍由既有 codec 无损兼容。
fn validate_group_title(
    raw_title: &str,
    groups: &[WorkspaceTabGroupProjection],
    excluded_group_id: Option<&str>,
) -> Result<String, GroupTitleError> {
    let title = validate_workspace_title(raw_title)?;
    let normalized = title.to_lowercase();
    if groups.iter().any(|group| {
        Some(group.id.as_str()) != excluded_group_id
            && group.title.to_string().trim().to_lowercase() == normalized
    }) {
        return Err(GroupTitleError::Duplicate);
    }
    Ok(title)
}

/// 标签与分组名称共用空名和控制字符约束；标签标题本身允许重复。
fn validate_workspace_title(raw_title: &str) -> Result<String, GroupTitleError> {
    let title = raw_title.trim();
    if title.is_empty() {
        return Err(GroupTitleError::Empty);
    }
    if title.chars().any(char::is_control) {
        return Err(GroupTitleError::ControlCharacter);
    }
    Ok(title.to_owned())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GroupTitleError {
    Empty,
    ControlCharacter,
    Duplicate,
}

/// 将校验原因直接映射为本地化表单反馈，避免无效创建被静默忽略。
fn workspace_name_error_text(error: GroupTitleError) -> SharedString {
    match error {
        GroupTitleError::Empty => crate::locale::text("请填写名称。", "Enter a name."),
        GroupTitleError::ControlCharacter => crate::locale::text(
            "名称不能包含控制字符。",
            "Names cannot contain control characters.",
        ),
        GroupTitleError::Duplicate => crate::locale::text(
            "已有同名分组，请换一个名称。",
            "A group with that name already exists.",
        ),
    }
}

/// 将冻结 v3 颜色映射为浅深主题均可见的强调色，未知值仍保留中性外观。
fn tab_group_color_tint(
    color: Option<&WorkspaceTabGroupColor>,
    theme: ThemeTokens,
) -> gpui_kit::Hsla {
    match color {
        Some(WorkspaceTabGroupColor::Blue) => gpui_kit::rgb(0x3b82f6).into(),
        Some(WorkspaceTabGroupColor::Pink) => gpui_kit::rgb(0xec4899).into(),
        Some(WorkspaceTabGroupColor::Purple) => gpui_kit::rgb(0x8b5cf6).into(),
        Some(WorkspaceTabGroupColor::Mint) => gpui_kit::rgb(0x10b981).into(),
        Some(WorkspaceTabGroupColor::Amber) => gpui_kit::rgb(0xf59e0b).into(),
        Some(WorkspaceTabGroupColor::Teal) => gpui_kit::rgb(0x14b8a6).into(),
        Some(WorkspaceTabGroupColor::Orange) => gpui_kit::rgb(0xf97316).into(),
        Some(WorkspaceTabGroupColor::Gray | WorkspaceTabGroupColor::Unsupported(_)) | None => {
            theme.muted
        }
    }
}

/// 组颜色菜单和回归测试共用同一份类型化选项，避免颜色标签与持久化事件错配。
///
/// @author kongweiguang
fn group_color_actions(group_id: &str) -> Vec<(&'static str, &'static str, WorkspaceTabEvent)> {
    [
        ("默认", "Default", None),
        ("蓝", "Blue", Some(WorkspaceTabGroupColor::Blue)),
        ("粉", "Pink", Some(WorkspaceTabGroupColor::Pink)),
        ("紫", "Purple", Some(WorkspaceTabGroupColor::Purple)),
        ("薄荷", "Mint", Some(WorkspaceTabGroupColor::Mint)),
        ("琥珀", "Amber", Some(WorkspaceTabGroupColor::Amber)),
        ("青", "Teal", Some(WorkspaceTabGroupColor::Teal)),
        ("橙", "Orange", Some(WorkspaceTabGroupColor::Orange)),
        ("灰", "Gray", Some(WorkspaceTabGroupColor::Gray)),
    ]
    .into_iter()
    .map(|(label_zh, label_en, color)| {
        (
            label_zh,
            label_en,
            WorkspaceTabEvent::SetGroupColor {
                group_id: group_id.to_owned(),
                color,
            },
        )
    })
    .collect()
}

/// 命名组始终显示；只有存在命名组时才展示未分组标题或临时空 drop target，避免普通工作区多一行。
/// 空目标借用 GPUI 活动拖拽状态而不维护第二套状态；查询不区分拖拽类型，但 handler 仍只处理 TabId。
///
/// @author kongweiguang
fn should_show_workspace_group_header(
    group_id: Option<&str>,
    has_groups: bool,
    has_tabs: bool,
    has_active_drag: bool,
) -> bool {
    group_id.is_some() || (has_groups && (has_tabs || has_active_drag))
}

/// 空白未分组区域只在存在命名组且任意拖拽期间扩展，防止空闲标签栏持续占空间。
/// GPUI 不暴露活动 drag payload 类型，因此实际 hover/drop handler 仍严格使用 TabId。
///
/// @author kongweiguang
fn should_enable_ungrouped_drop_target(has_groups: bool, has_active_drag: bool) -> bool {
    has_groups && has_active_drag
}

/// 描述滚动轨道宽度预算需要的可见组头、固定宽标签和末尾投放区。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WorkspaceTabTrackSection {
    pub(super) header_text_units: Option<usize>,
    pub(super) collapsed_header: bool,
    pub(super) visible_tab_count: usize,
    pub(super) is_ungrouped_drop_target: bool,
}

/// 按标签实际固定宽度及组头保守预算设置内容轨道，避免后代溢出脱离 GPUI 的直接子项滚动测量。
/// 普通态预算完整可见内容，`min_w_full` 负责视口下限；仅活动拖拽把末尾 72px 投放区计入预算。
/// 组头预算按每 UTF-16 单元 16px 并含按钮/标记固定留量，覆盖宽字符且避免文本溢出截断末尾命中区。
///
/// @author kongweiguang
pub(super) fn workspace_tab_track_width_budget(sections: &[WorkspaceTabTrackSection]) -> f32 {
    const TAB_WIDTH: usize = 180;
    const TAB_GAP: usize = 1;
    const TAB_STRIP_PADDING: usize = 16;
    const SECTION_GAP: usize = 4;
    const HEADER_GAP: usize = 4;
    const HEADER_CHROME: usize = 72;
    const COLLAPSED_HEADER_STATUS_WIDTH: usize = 24;
    const HEADER_CHAR_WIDTH: usize = 16;
    const DROP_TARGET_WIDTH: usize = 72;

    let mut track_width = 0usize;
    let mut rendered_sections = 0usize;
    for section in sections {
        let header_width = section.header_text_units.map_or(0, |text_units| {
            HEADER_CHROME
                .saturating_add(if section.collapsed_header {
                    COLLAPSED_HEADER_STATUS_WIDTH
                } else {
                    0
                })
                .saturating_add(text_units.saturating_mul(HEADER_CHAR_WIDTH))
        });
        let tabs_width = if section.visible_tab_count == 0 {
            0
        } else {
            TAB_STRIP_PADDING
                .saturating_add(section.visible_tab_count.saturating_mul(TAB_WIDTH))
                .saturating_add(
                    section
                        .visible_tab_count
                        .saturating_sub(1)
                        .saturating_mul(TAB_GAP),
                )
        };
        let mut section_width = header_width.saturating_add(tabs_width);
        if header_width > 0 && tabs_width > 0 {
            section_width = section_width.saturating_add(HEADER_GAP);
        }
        if section.is_ungrouped_drop_target {
            section_width = section_width.saturating_add(DROP_TARGET_WIDTH);
        }
        if section_width > 0 {
            if rendered_sections > 0 {
                track_width = track_width.saturating_add(SECTION_GAP);
            }
            track_width = track_width.saturating_add(section_width);
            rendered_sections += 1;
        }
    }
    track_width as f32
}

#[cfg(test)]
#[path = "../../tests/unit/workspace_tabs.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/unit/workspace_tabs_drag.rs"]
mod drag_tests;
