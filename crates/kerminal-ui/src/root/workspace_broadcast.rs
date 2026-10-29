//! Root owns per-Tab broadcast policy and connects the shared toolbar to existing session services.
//!
//! @author kongweiguang

use std::collections::{BTreeSet, HashMap};

use gpui_kit::{AnyElement, AppContext as _, Context, Entity, IntoElement, Subscription, Window};
use kerminal_application::{
    commands::{BroadcastDeliveryResult, BroadcastSessionBinding},
    sessions::UiSessionBinding,
    workspace::{WorkspaceSessionSnapshot, WorkspaceSessionTab},
};
use kerminal_domain::{
    commands::{BroadcastAnalysis, BroadcastTarget, CommandHistoryTarget},
    identity::{PaneId, TabId},
    sessions::{SessionOwner, SessionTarget},
    workspace::TerminalLayoutNode,
};
use tokio_util::sync::CancellationToken;

use crate::{
    features::{
        workspace_broadcast::{
            PaneBroadcastProjection, WorkspaceBroadcastBar, WorkspaceBroadcastBarEvent,
            WorkspaceBroadcastBarProjection, WorkspaceBroadcastDeliveryProjection,
            WorkspaceBroadcastTargetMode, WorkspaceBroadcastTargetProjection,
            broadcast_draft_error,
        },
        workspace_panes::WorkspacePaneEvent,
    },
    root::{CenterPanel, RootView, terminal_tool_actions},
};

/// 共享工具栏只持有各 Tab 的交互状态，不拥有可见 UI pane 之外的会话。
pub(super) struct WorkspaceBroadcastRuntime {
    pub(super) bar: Entity<WorkspaceBroadcastBar>,
    _bar_subscription: Subscription,
    tabs: HashMap<TabId, WorkspaceBroadcastTabState>,
    last_active_tab_id: Option<TabId>,
    next_request_id: u64,
    sync_validation: Option<PendingSyncValidation>,
    active_command: Option<ActiveWorkspaceCommand>,
}

struct WorkspaceBroadcastTabState {
    mode: WorkspaceBroadcastTargetMode,
    custom_panes: BTreeSet<PaneId>,
    draft: String,
    bar_visible: bool,
    previous_view_pane_count: usize,
    sync_enabled: bool,
    status: Option<String>,
    status_is_error: bool,
    command_request_id: Option<u64>,
    last_result: Option<WorkspaceBroadcastResult>,
}

#[derive(Clone)]
struct FrozenBroadcastRequest {
    tab_id: TabId,
    command: String,
    draft_snapshot: String,
    targets: Vec<BroadcastTarget>,
    ui_bindings: Vec<UiSessionBinding>,
    api_bindings: Vec<BroadcastSessionBinding>,
}

#[derive(Clone)]
struct WorkspaceBroadcastResult {
    request: FrozenBroadcastRequest,
    deliveries: Vec<BroadcastDeliveryResult>,
}

struct PendingSyncValidation {
    generation: u64,
    tab_id: TabId,
    bindings: Vec<UiSessionBinding>,
    cancellation: CancellationToken,
}

struct ActiveWorkspaceCommand {
    request_id: u64,
    tab_id: TabId,
    cancellation: CancellationToken,
    request: FrozenBroadcastRequest,
}

impl WorkspaceBroadcastTabState {
    /// 默认全选以自然纳入新分屏；用户改选时再转为固定的自选集合。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            mode: WorkspaceBroadcastTargetMode::All,
            custom_panes: BTreeSet::new(),
            draft: String::new(),
            bar_visible: true,
            previous_view_pane_count: 0,
            sync_enabled: false,
            status: None,
            status_is_error: false,
            command_request_id: None,
            last_result: None,
        }
    }

    /// 短暂操作状态与发送回执分开保存，避免同步提示或输入校验覆盖仍可追溯的结果。
    ///
    /// @author kongweiguang
    fn set_status(&mut self, message: impl Into<String>, is_error: bool) {
        self.status = Some(message.into());
        self.status_is_error = is_error;
    }

    /// 清除短暂状态时保留发送回执，结果只在下一次发送真正开始时失效。
    ///
    /// @author kongweiguang
    fn clear_status(&mut self) {
        self.status = None;
        self.status_is_error = false;
    }

    /// 新发送开始时原子更新请求代次并清除旧回执，避免迟到结果或编辑状态混淆当前发送。
    ///
    /// @author kongweiguang
    fn begin_command(&mut self, request_id: u64, progress: &str) {
        self.command_request_id = Some(request_id);
        self.last_result = None;
        self.set_status(progress, false);
    }

    /// 超限粘贴保留原文供修正；合法编辑结束短暂错误，但保留上次发送结果供用户复查。
    ///
    /// @author kongweiguang
    fn replace_draft(&mut self, draft: String, progress: Option<&str>) {
        self.draft = draft;
        self.clear_status();
        if let Some(error) = broadcast_draft_error(&self.draft) {
            self.set_status(error.to_string(), true);
        } else if let Some(progress) = progress {
            self.set_status(progress, false);
        }
    }
}

/// 外层执行失败也保留冻结目标；空回执代表结果未知，编辑状态不能清除它。
///
/// @author kongweiguang
fn finish_workspace_broadcast_error(
    state: &mut WorkspaceBroadcastTabState,
    request: FrozenBroadcastRequest,
    message: &str,
) {
    state.last_result = Some(WorkspaceBroadcastResult {
        request,
        deliveries: Vec::new(),
    });
    state.set_status(message, true);
}

impl WorkspaceBroadcastResult {
    /// 按发送时冻结的目标集合统计回执；缺失、重复或外来回执都不能伪装成完整成功。
    ///
    /// @author kongweiguang
    fn summary(&self) -> Option<String> {
        let mut sent = 0;
        let mut attempted_failed = 0;
        let mut not_sent = 0;
        let mut receipt_unconfirmed = 0;
        let mut history_unconfirmed = 0;
        for target in &self.request.targets {
            let Some(delivery) = unique_broadcast_delivery(target, &self.deliveries) else {
                receipt_unconfirmed += 1;
                continue;
            };
            if delivery.sent {
                sent += 1;
            } else if delivery.attempted {
                attempted_failed += 1;
            } else {
                not_sent += 1;
            }
            if delivery.history_error_code.is_some() {
                history_unconfirmed += 1;
            }
        }
        receipt_unconfirmed += self
            .deliveries
            .iter()
            .filter(|delivery| {
                !self
                    .request
                    .targets
                    .iter()
                    .any(|target| target.pane_id == delivery.pane_id)
            })
            .count();
        let all_sent = result_all_targets_sent(&self.request.targets, &self.deliveries);
        if all_sent && history_unconfirmed == 0 && receipt_unconfirmed == 0 {
            return None;
        }

        Some(
            crate::locale::text(
                format!(
                    "已发送 {sent}/{}；结果未知或失败 {attempted_failed}，未发送 {not_sent}，回执未确认 {receipt_unconfirmed}，历史记录未确认 {history_unconfirmed}。",
                    self.request.targets.len()
                ),
                format!(
                    "Sent {sent}/{}; {attempted_failed} outcomes failed or are unknown, {not_sent} were not sent, {receipt_unconfirmed} delivery receipts were unconfirmed, and {history_unconfirmed} history records were not confirmed.",
                    self.request.targets.len()
                ),
            )
            .to_string(),
        )
    }
}

impl PendingSyncValidation {
    /// 撤销校验同时结束对应 Tab 的进度反馈；迟到回执不会再负责替用户收敛状态。
    ///
    /// @author kongweiguang
    fn cancel(self, state: Option<&mut WorkspaceBroadcastTabState>) {
        self.cancellation.cancel();
        if let Some(state) = state {
            state.sync_enabled = false;
            state.set_status("同步按键已关闭。", false);
        }
    }
}

impl RootView {
    /// 在实际 GPUI Window 中创建输入实体，避免后台构造 InputState 导致焦点和 IME 状态无效。
    ///
    /// @author kongweiguang
    pub(super) fn init_workspace_broadcast(
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> WorkspaceBroadcastRuntime {
        let bar = cx.new(|cx| WorkspaceBroadcastBar::new(window, cx));
        let subscription = cx.subscribe_in(&bar, window, |root, _, event, window, cx| {
            root.handle_workspace_broadcast_bar_event(event.clone(), window, cx);
        });
        WorkspaceBroadcastRuntime {
            bar,
            _bar_subscription: subscription,
            tabs: HashMap::new(),
            last_active_tab_id: None,
            next_request_id: 0,
            sync_validation: None,
            active_command: None,
        }
    }

    /// 只返回同一 Tab 中仍属 Ready pane 且 owner/generation 未变化的输入绑定。
    ///
    /// @author kongweiguang
    pub(super) fn workspace_broadcast_input_binding(
        &self,
        tab_id: &TabId,
        pane_id: &PaneId,
    ) -> Option<UiSessionBinding> {
        let pane = self.workspace_runtime.pane(pane_id)?;
        if pane.tab_id != *tab_id || pane.phase != super::workspace_runtime::RuntimePanePhase::Ready
        {
            return None;
        }
        let session_id = pane.session_id.as_ref()?;
        let registration = self.pane_input_handlers.get(session_id)?;
        let binding = &registration.binding;
        (binding.session_id == *session_id
            && binding.owner
                == (SessionOwner::Ui {
                    tab_id: tab_id.clone(),
                    pane_id: pane_id.clone(),
                }))
        .then(|| binding.clone())
    }

    /// 页头只接收模式投影，不自行推断会话身份或运行状态。
    ///
    /// @author kongweiguang
    pub(super) fn workspace_broadcast_header_projection(
        &self,
        tab_id: &TabId,
        pane_id: &PaneId,
        view_pane_count: usize,
        focused_pane_id: Option<&PaneId>,
        input_enabled: bool,
    ) -> PaneBroadcastProjection {
        let Some(state) = self.workspace_broadcast.tabs.get(tab_id) else {
            return PaneBroadcastProjection {
                visible: false,
                enabled: input_enabled,
                ..PaneBroadcastProjection::default()
            };
        };
        let selected = match state.mode {
            WorkspaceBroadcastTargetMode::All => input_enabled,
            WorkspaceBroadcastTargetMode::Current => {
                input_enabled && focused_pane_id == Some(pane_id)
            }
            WorkspaceBroadcastTargetMode::Custom => state.custom_panes.contains(pane_id),
        };
        PaneBroadcastProjection {
            visible: view_pane_count >= 2 && state.bar_visible,
            selected,
            enabled: input_enabled,
        }
    }

    /// 按候选布局同步显隐和生命周期；状态未变化时不触发额外重绘。
    ///
    /// @author kongweiguang
    pub(super) fn reconcile_workspace_broadcast(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self
            .workspace_controller
            .as_ref()
            .map(|controller| controller.snapshot().clone());
        let Some(snapshot) = snapshot else {
            self.stop_workspace_broadcast_for_lifecycle("工作区不可用，已停止同步输入。");
            return;
        };
        let pending_split = self.pending_workspace_split_preview(cx);
        let mut view_counts = HashMap::new();
        let mut view_panes_by_tab = HashMap::new();
        let mut live_terminal_tabs = BTreeSet::new();
        for tab in &snapshot.terminal_tabs {
            let WorkspaceSessionTab::Terminal { id, layout, .. } = tab else {
                continue;
            };
            live_terminal_tabs.insert(id.clone());
            if let Some((view_layout, _, _)) = Self::workspace_render_layout_projection(
                &snapshot,
                id,
                layout,
                pending_split.as_ref(),
            ) {
                let pane_ids = view_layout.pane_ids();
                view_counts.insert(id.clone(), pane_ids.len());
                view_panes_by_tab.insert(id.clone(), pane_ids);
            }
        }
        let active_tab = (self.center_panel == CenterPanel::Terminal)
            .then(|| snapshot.active_tab_id.clone())
            .flatten()
            .filter(|tab_id| live_terminal_tabs.contains(tab_id));

        let switched = self.workspace_broadcast.last_active_tab_id != active_tab;
        if switched {
            if self.broadcast_session_ids.is_some() || self.pending_broadcast_generation.is_some() {
                let _ = self.stop_broadcast();
            }
            self.cancel_pending_sync_validation();
            let previous_active_tab_id = self.workspace_broadcast.last_active_tab_id.clone();
            self.cancel_active_workspace_command_for_tab(
                previous_active_tab_id.as_ref(),
                "切换工作区标签后已停止后续群发目标。",
            );
            self.workspace_broadcast.last_active_tab_id = active_tab.clone();
        }

        self.workspace_broadcast
            .tabs
            .retain(|tab_id, _| live_terminal_tabs.contains(tab_id));
        for tab_id in live_terminal_tabs {
            let pane_ids = view_panes_by_tab.remove(&tab_id).unwrap_or_default();
            let state = self
                .workspace_broadcast
                .tabs
                .entry(tab_id)
                .or_insert_with(WorkspaceBroadcastTabState::default);
            reconcile_workspace_broadcast_tab_state(state, &pane_ids);
        }

        let invalid_sync_validation = self
            .workspace_broadcast
            .sync_validation
            .as_ref()
            .is_some_and(|pending| {
                let bar_visible = self
                    .workspace_broadcast
                    .tabs
                    .get(&pending.tab_id)
                    .is_some_and(|state| state.bar_visible)
                    && view_counts
                        .get(&pending.tab_id)
                        .copied()
                        .unwrap_or_default()
                        >= 2;
                let bindings_current = self
                    .workspace_broadcast_committed_bindings(&pending.tab_id, &snapshot)
                    .is_some_and(|selected| {
                        workspace_broadcast_bindings_match_selection(&pending.bindings, &selected)
                    });
                should_cancel_pending_sync_validation(
                    active_tab.as_ref(),
                    &pending.tab_id,
                    bar_visible,
                    bindings_current,
                )
            });
        if invalid_sync_validation {
            let pending_generation = self
                .workspace_broadcast
                .sync_validation
                .as_ref()
                .map(|pending| pending.generation);
            if let Some(generation) = pending_generation {
                self.reject_pending_workspace_broadcast_sync(
                    generation,
                    "同步栏、目标或连接已变化，未开启同步按键。",
                );
            }
        }

        let invalid_command_tab = self
            .workspace_broadcast
            .active_command
            .as_ref()
            .filter(|active| {
                !active.cancellation.is_cancelled()
                    && should_cancel_active_workspace_command(
                        active_tab.as_ref(),
                        &active.tab_id,
                        self.workspace_broadcast
                            .tabs
                            .get(&active.tab_id)
                            .is_some_and(|state| {
                                state.bar_visible
                                    && view_counts.get(&active.tab_id).copied().unwrap_or_default()
                                        >= 2
                            }),
                        self.workspace_broadcast_request_bindings_are_current(&active.request),
                    )
            })
            .map(|active| active.tab_id.clone());
        if let Some(tab_id) = invalid_command_tab {
            self.cancel_active_workspace_command_for_tab(
                Some(&tab_id),
                "群发目标连接或 pane 已变化；已停止后续发送。",
            );
        }

        if let Some(tab_id) = active_tab.as_ref()
            && self.broadcast_session_ids.is_some()
            && !self.workspace_broadcast_targets_are_current(tab_id, &snapshot)
        {
            self.stop_workspace_broadcast_for_lifecycle(
                "群发目标连接或选择已变化，同步输入已停止。",
            );
            if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
                state.set_status("群发目标连接或选择已变化，同步输入已停止。", true);
            }
        }
        self.sync_bar_projection(&snapshot, active_tab.as_ref(), _window, cx);
    }

    /// 首次绘制前复用候选布局决定工具栏显隐，保证测量时已包含其占用空间。
    ///
    /// @author kongweiguang
    fn workspace_broadcast_view_layout(
        &self,
        snapshot: &WorkspaceSessionSnapshot,
        tab_id: &TabId,
        cx: &Context<Self>,
    ) -> Option<(TerminalLayoutNode, PaneId)> {
        let tab = snapshot
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == tab_id)?;
        let WorkspaceSessionTab::Terminal { layout, .. } = tab else {
            return None;
        };
        let pending_split = self.pending_workspace_split_preview(cx);
        let (layout, focused, _) = Self::workspace_render_layout_projection(
            snapshot,
            tab_id,
            layout,
            pending_split.as_ref(),
        )?;
        Some((layout, focused))
    }

    /// 仅在当前终端 Tab 的候选分屏布局上方渲染共享工具栏。
    ///
    /// @author kongweiguang
    pub(super) fn render_workspace_broadcast_bar(
        &mut self,
        tab_id: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let snapshot = self.workspace_controller.as_ref()?.snapshot().clone();
        if snapshot.active_tab_id.as_ref() != Some(tab_id)
            || self.center_panel != CenterPanel::Terminal
        {
            return None;
        }
        let (layout, _) = self.workspace_broadcast_view_layout(&snapshot, tab_id, cx)?;
        if layout.pane_ids().len() < 2 {
            return None;
        }
        let state = self.workspace_broadcast.tabs.get(tab_id)?;
        if !state.bar_visible {
            return None;
        }
        let projection = self.bar_projection(tab_id, &layout, cx);
        self.workspace_broadcast.bar.update(cx, |bar, cx| {
            bar.set_projection(projection, window, cx);
        });
        Some(self.workspace_broadcast.bar.clone().into_any_element())
    }

    /// 页头与菜单共用目标集合；工具栏随多分屏自动显示，页头入口只负责聚焦命令输入。
    ///
    /// @author kongweiguang
    pub(super) fn handle_workspace_broadcast_header_event(
        &mut self,
        tab_id: &TabId,
        event: WorkspacePaneEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            WorkspacePaneEvent::ToggleBroadcastTarget { pane_id } => {
                self.toggle_workspace_broadcast_target(tab_id, &pane_id, cx);
            }
            WorkspacePaneEvent::ShowBroadcast { pane_id } => {
                let can_reopen = self.workspace_runtime.pane(&pane_id).is_some_and(|pane| {
                    pane.tab_id == *tab_id
                        && pane.phase == super::workspace_runtime::RuntimePanePhase::Ready
                }) && self.workspace_controller.as_ref().is_some_and(
                    |controller| {
                        self.workspace_broadcast_view_layout(controller.snapshot(), tab_id, cx)
                            .is_some_and(|(layout, _)| {
                                layout.pane_ids().contains(&pane_id) && layout.pane_ids().len() >= 2
                            })
                    },
                );
                if can_reopen && let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
                    state.bar_visible = true;
                    state.clear_status();
                }
                let is_active = can_reopen
                    && self
                        .workspace_controller
                        .as_ref()
                        .is_some_and(|controller| {
                            controller.snapshot().active_tab_id.as_ref() == Some(tab_id)
                        });
                if is_active {
                    self.workspace_broadcast.bar.update(cx, |bar, cx| {
                        bar.focus_draft(window, cx);
                    });
                }
                cx.notify();
            }
            _ => {}
        }
    }

    /// 将工具面板 Stop 与队列失败同步回工具栏，避免再次调用 Root Stop 形成递归。
    ///
    /// @author kongweiguang
    pub(super) fn on_root_broadcast_stopped(&mut self) {
        self.cancel_pending_sync_validation();
        for state in self.workspace_broadcast.tabs.values_mut() {
            state.sync_enabled = false;
        }
    }

    /// 菜单与页头共用精确绑定下的选中集合；结果异常与短暂状态分别投影。
    ///
    /// @author kongweiguang
    fn bar_projection(
        &self,
        tab_id: &TabId,
        layout: &TerminalLayoutNode,
        cx: &Context<Self>,
    ) -> WorkspaceBroadcastBarProjection {
        let Some(state) = self.workspace_broadcast.tabs.get(tab_id) else {
            return WorkspaceBroadcastBarProjection::default();
        };
        let selected = self.selected_workspace_pane_ids(tab_id, layout, None);
        let focused = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.snapshot().focused_pane_id.as_ref());
        let targets = layout
            .pane_ids()
            .into_iter()
            .map(|pane_id| {
                let enabled = self
                    .workspace_broadcast_input_binding(tab_id, &pane_id)
                    .is_some();
                let pane = self.workspace_runtime.pane(&pane_id);
                let protocol = pane
                    .and_then(|pane| pane.target.as_ref())
                    .map_or("", |target| match target {
                        SessionTarget::Local(_) => "LOCAL",
                        SessionTarget::Ssh(_) => "SSH",
                        SessionTarget::Telnet(_) => "TELNET",
                        SessionTarget::Serial(_) => "SERIAL",
                        SessionTarget::Container(_) => "CONTAINER",
                    });
                WorkspaceBroadcastTargetProjection {
                    title: self.workspace_pane_title(&pane_id, cx).to_string(),
                    detail: pane
                        .and_then(|pane| pane.current_working_directory.as_deref())
                        .unwrap_or_default()
                        .to_owned(),
                    protocol: protocol.to_owned(),
                    selected: enabled && selected.contains(&pane_id),
                    enabled,
                    current: focused == Some(&pane_id),
                    pane_id,
                }
            })
            .collect::<Vec<_>>();
        let selected_count = targets.iter().filter(|target| target.selected).count();
        WorkspaceBroadcastBarProjection {
            tab_id: Some(tab_id.clone()),
            pane_count: layout.pane_ids().len(),
            selected_count,
            mode: state.mode,
            targets,
            draft: state.draft.clone(),
            sync_enabled: state.sync_enabled,
            command_pending: self
                .workspace_broadcast
                .active_command
                .as_ref()
                .is_some_and(|active| active.tab_id == *tab_id),
            execution_busy: self.workspace_broadcast.active_command.is_some(),
            result_summary: state
                .last_result
                .as_ref()
                .and_then(WorkspaceBroadcastResult::summary),
            result_details: state
                .last_result
                .as_ref()
                .map(result_delivery_details)
                .unwrap_or_default(),
            status: state.status.clone(),
            status_is_error: state.status_is_error,
        }
    }

    /// 将选择模式解析为稳定 pane ID；标题只用于显示，不作为目标身份。
    fn selected_workspace_pane_ids(
        &self,
        tab_id: &TabId,
        layout: &TerminalLayoutNode,
        focused_override: Option<&PaneId>,
    ) -> Vec<PaneId> {
        let Some(state) = self.workspace_broadcast.tabs.get(tab_id) else {
            return Vec::new();
        };
        let focused = focused_override.or_else(|| {
            self.workspace_controller
                .as_ref()
                .and_then(|controller| controller.snapshot().focused_pane_id.as_ref())
        });
        select_workspace_broadcast_panes(
            state.mode,
            &state.custom_panes,
            &layout.pane_ids(),
            focused,
        )
    }

    /// 菜单与页头改选都只冻结当下可输入目标，候选 pane 不会在就绪后悄悄加入自选。
    ///
    /// @author kongweiguang
    fn toggle_workspace_broadcast_target(
        &mut self,
        tab_id: &TabId,
        pane_id: &PaneId,
        cx: &mut Context<Self>,
    ) {
        let Some((layout, focused)) = self.workspace_controller.as_ref().and_then(|controller| {
            self.workspace_broadcast_view_layout(controller.snapshot(), tab_id, cx)
        }) else {
            return;
        };
        if !layout.contains_pane(pane_id) {
            return;
        }
        let selected = self
            .selected_workspace_pane_ids(tab_id, &layout, Some(&focused))
            .into_iter()
            .filter(|pane_id| {
                self.workspace_broadcast_input_binding(tab_id, pane_id)
                    .is_some()
            })
            .collect::<BTreeSet<_>>();
        let input_enabled = self
            .workspace_broadcast_input_binding(tab_id, pane_id)
            .is_some();
        let state = self
            .workspace_broadcast
            .tabs
            .entry(tab_id.clone())
            .or_insert_with(WorkspaceBroadcastTabState::default);
        if state.mode != WorkspaceBroadcastTargetMode::Custom {
            state.custom_panes = selected;
            state.mode = WorkspaceBroadcastTargetMode::Custom;
        }
        if state.custom_panes.contains(pane_id) {
            state.custom_panes.remove(pane_id);
        } else if input_enabled {
            state.custom_panes.insert(pane_id.clone());
        }
        self.stop_workspace_broadcast_for_selection_change(tab_id);
        cx.notify();
    }

    /// 用户改选后使旧校验和固定目标失效，避免同步路由沿用过期集合。
    fn stop_workspace_broadcast_for_selection_change(&mut self, tab_id: &TabId) {
        self.cancel_active_workspace_command_for_tab(
            Some(tab_id),
            "目标选择已变化；正在停止后续发送。",
        );
        if self.workspace_broadcast.sync_validation.is_some() {
            self.cancel_pending_sync_validation();
            if self.pending_broadcast_generation.is_some() {
                let _ = self.stop_broadcast();
            }
        }
        if self
            .workspace_broadcast
            .tabs
            .get(tab_id)
            .is_some_and(|state| state.sync_enabled)
        {
            let _ = self.stop_broadcast();
        }
    }

    /// Tab、目标或 pane 生命周期使冻结绑定失效时，停止同步并取消仍在运行的命令。
    fn stop_workspace_broadcast_for_lifecycle(&mut self, message: &str) {
        if self.broadcast_session_ids.is_some() || self.pending_broadcast_generation.is_some() {
            let _ = self.stop_broadcast();
        } else {
            self.on_root_broadcast_stopped();
        }
        if let Some(active) = self.workspace_broadcast.active_command.as_ref() {
            active.cancellation.cancel();
            if let Some(state) = self.workspace_broadcast.tabs.get_mut(&active.tab_id) {
                state.set_status(message, true);
            }
        }
    }

    /// 忙碌提示保留每个 Tab 的草稿，等待当前逐目标回执收敛后再操作。
    ///
    /// @author kongweiguang
    fn report_workspace_broadcast_busy(&mut self, tab_id: &TabId) {
        if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
            state.set_status("另一项群发仍在收尾，请稍后重试。", false);
        }
    }

    /// 只收敛匹配代次的同步校验，避免迟到回执取消更新一轮的广播启动。
    ///
    /// @author kongweiguang
    fn reject_pending_workspace_broadcast_sync(&mut self, generation: u64, message: &str) {
        let Some(tab_id) = self
            .workspace_broadcast
            .sync_validation
            .as_ref()
            .filter(|pending| pending.generation == generation)
            .map(|pending| pending.tab_id.clone())
        else {
            return;
        };
        self.cancel_pending_sync_validation();
        if self.pending_broadcast_generation == Some(generation) {
            let _ = self.stop_broadcast();
        }
        if let Some(state) = self.workspace_broadcast.tabs.get_mut(&tab_id) {
            state.set_status(message, true);
        }
    }

    /// 同步路由必须精确等于当前模式选出的完整集合；集合或任一 binding 变化都停止输入扇出。
    ///
    /// @author kongweiguang
    fn workspace_broadcast_targets_are_current(
        &self,
        tab_id: &TabId,
        snapshot: &WorkspaceSessionSnapshot,
    ) -> bool {
        let Some(active) = self.broadcast_session_ids.as_ref() else {
            return false;
        };
        let Some(selected) = self.workspace_broadcast_committed_bindings(tab_id, snapshot) else {
            return false;
        };
        workspace_broadcast_bindings_match_selection(active, &selected)
            && active.iter().all(|binding| {
                let SessionOwner::Ui {
                    tab_id: owner_tab,
                    pane_id,
                } = &binding.owner
                else {
                    return false;
                };
                owner_tab == tab_id
                    && self
                        .workspace_broadcast_input_binding(tab_id, pane_id)
                        .as_ref()
                        == Some(binding)
            })
    }

    /// 目标集合只从已提交的终端布局解析，候选分屏不会提前成为同步输入目标。
    ///
    /// @author kongweiguang
    fn workspace_broadcast_committed_bindings(
        &self,
        tab_id: &TabId,
        snapshot: &WorkspaceSessionSnapshot,
    ) -> Option<Vec<UiSessionBinding>> {
        let tab = snapshot
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == tab_id)?;
        let WorkspaceSessionTab::Terminal { layout, .. } = tab else {
            return None;
        };
        Some(self.selected_workspace_bindings(tab_id, layout))
    }

    /// Root 以投影驱动工具栏；普通切 Tab 保留每个 Tab 的草稿和请求状态。
    fn sync_bar_projection(
        &mut self,
        snapshot: &WorkspaceSessionSnapshot,
        active_tab: Option<&TabId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let projection = active_tab
            .and_then(|tab_id| {
                let (layout, _) = self.workspace_broadcast_view_layout(snapshot, tab_id, cx)?;
                let state = self.workspace_broadcast.tabs.get(tab_id)?;
                (state.bar_visible && layout.pane_ids().len() >= 2)
                    .then(|| self.bar_projection(tab_id, &layout, cx))
            })
            .unwrap_or_default();
        self.workspace_broadcast.bar.update(cx, |bar, cx| {
            bar.set_projection(projection, window, cx);
        });
    }

    /// 工具栏事件先校验捕获的 Tab，再更新状态或启动异步操作，防止旧投影写入当前 Tab。
    ///
    /// @author kongweiguang
    fn handle_workspace_broadcast_bar_event(
        &mut self,
        event: WorkspaceBroadcastBarEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab_id = match &event {
            WorkspaceBroadcastBarEvent::DraftChanged { tab_id, .. }
            | WorkspaceBroadcastBarEvent::SetMode { tab_id, .. }
            | WorkspaceBroadcastBarEvent::ToggleTarget { tab_id, .. }
            | WorkspaceBroadcastBarEvent::ToggleSync { tab_id }
            | WorkspaceBroadcastBarEvent::SendCommand { tab_id } => tab_id.clone(),
        };
        if self.workspace_broadcast.last_active_tab_id.as_ref() != Some(&tab_id) {
            return;
        }
        match event {
            WorkspaceBroadcastBarEvent::DraftChanged { draft, .. } => {
                let progress = if self
                    .workspace_broadcast
                    .sync_validation
                    .as_ref()
                    .is_some_and(|pending| pending.tab_id == tab_id)
                {
                    Some("正在校验同步目标…")
                } else if self
                    .workspace_broadcast
                    .active_command
                    .as_ref()
                    .is_some_and(|active| active.tab_id == tab_id)
                {
                    Some("群发处理中…")
                } else {
                    None
                };
                if let Some(state) = self.workspace_broadcast.tabs.get_mut(&tab_id) {
                    state.replace_draft(draft, progress);
                }
            }
            WorkspaceBroadcastBarEvent::SetMode { mode, .. } => {
                self.set_workspace_broadcast_mode(&tab_id, mode, cx);
            }
            WorkspaceBroadcastBarEvent::ToggleTarget { pane_id, .. } => {
                self.toggle_workspace_broadcast_target(&tab_id, &pane_id, cx);
            }
            WorkspaceBroadcastBarEvent::ToggleSync { .. } => {
                self.toggle_workspace_broadcast_sync(&tab_id, cx);
            }
            WorkspaceBroadcastBarEvent::SendCommand { .. } => {
                self.send_workspace_broadcast_command(&tab_id, cx);
            }
        }
        self.sync_bar_projection_for_current_tab(window, cx);
        cx.notify();
    }

    /// 切入自选保留当前已勾选的可输入集合；未就绪候选不视为用户已经选中。
    ///
    /// @author kongweiguang
    fn set_workspace_broadcast_mode(
        &mut self,
        tab_id: &TabId,
        mode: WorkspaceBroadcastTargetMode,
        cx: &mut Context<Self>,
    ) {
        if self
            .workspace_broadcast
            .tabs
            .get(tab_id)
            .is_some_and(|state| state.mode == mode)
        {
            return;
        }
        let Some((layout, focused)) = self.workspace_controller.as_ref().and_then(|controller| {
            self.workspace_broadcast_view_layout(controller.snapshot(), tab_id, cx)
        }) else {
            return;
        };
        let current = self
            .selected_workspace_pane_ids(tab_id, &layout, Some(&focused))
            .into_iter()
            .filter(|pane_id| {
                self.workspace_broadcast_input_binding(tab_id, pane_id)
                    .is_some()
            })
            .collect::<BTreeSet<_>>();
        let state = self
            .workspace_broadcast
            .tabs
            .entry(tab_id.clone())
            .or_insert_with(WorkspaceBroadcastTabState::default);
        if mode == WorkspaceBroadcastTargetMode::Custom {
            state.custom_panes = current;
        }
        state.mode = mode;
        self.stop_workspace_broadcast_for_selection_change(tab_id);
    }

    /// 用户操作后只更新工具栏投影，不唤醒无关工作区渲染。
    fn sync_bar_projection_for_current_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab_id) = self.workspace_broadcast.last_active_tab_id.clone() else {
            return;
        };
        let Some(snapshot) = self
            .workspace_controller
            .as_ref()
            .map(|controller| controller.snapshot().clone())
        else {
            return;
        };
        if let Some((layout, _)) = self.workspace_broadcast_view_layout(&snapshot, &tab_id, cx)
            && let Some(state) = self.workspace_broadcast.tabs.get(&tab_id)
            && state.bar_visible
        {
            let projection = self.bar_projection(&tab_id, &layout, cx);
            self.workspace_broadcast.bar.update(cx, |bar, cx| {
                bar.set_projection(projection, window, cx);
            });
        }
    }

    /// 按已选择的 UI panes 捕获精确 owner/generation；后台 Session 和 Agent CLI 不进入此集合。
    ///
    /// @author kongweiguang
    fn selected_workspace_bindings(
        &self,
        tab_id: &TabId,
        layout: &TerminalLayoutNode,
    ) -> Vec<UiSessionBinding> {
        self.selected_workspace_pane_ids(tab_id, layout, None)
            .into_iter()
            .filter_map(|pane_id| self.workspace_broadcast_input_binding(tab_id, &pane_id))
            .collect()
    }

    /// 从已绑定 UI pane 的真实 SessionTarget 生成应用广播三元组，不从标题或终端文本推测类型。
    ///
    /// @author kongweiguang
    fn freeze_workspace_broadcast_request(
        &self,
        tab_id: &TabId,
        command: String,
        draft_snapshot: String,
        ui_bindings: Vec<UiSessionBinding>,
        cx: &Context<Self>,
    ) -> Option<FrozenBroadcastRequest> {
        if ui_bindings.is_empty() || ui_bindings.len() > 64 {
            return None;
        }
        let mut targets = Vec::with_capacity(ui_bindings.len());
        let mut api_bindings = Vec::with_capacity(ui_bindings.len());
        for binding in &ui_bindings {
            let SessionOwner::Ui {
                tab_id: owner_tab,
                pane_id,
            } = &binding.owner
            else {
                return None;
            };
            if owner_tab != tab_id {
                return None;
            }
            let pane = self.workspace_runtime.pane(pane_id)?;
            if pane.tab_id != *tab_id
                || pane.phase != super::workspace_runtime::RuntimePanePhase::Ready
                || pane.session_id.as_ref() != Some(&binding.session_id)
            {
                return None;
            }
            let target = pane.target.as_ref()?;
            let history_target = history_target_for_session(target);
            targets.push(BroadcastTarget {
                pane_id: pane_id.clone(),
                title: self.workspace_pane_title(pane_id, cx).to_string(),
                target: history_target,
            });
            api_bindings.push(BroadcastSessionBinding {
                pane_id: pane_id.clone(),
                session_id: binding.session_id.clone(),
                target: history_target,
            });
        }
        for index in 0..targets.len() {
            let title = targets[index].title.clone();
            if targets
                .iter()
                .filter(|target| target.title == title)
                .count()
                > 1
            {
                targets[index].title = format!("{title} · {}", index + 1);
            }
        }
        let analysis = BroadcastAnalysis::analyze(&command, targets.clone());
        analysis.is_sendable().then(|| FrozenBroadcastRequest {
            tab_id: tab_id.clone(),
            command,
            draft_snapshot,
            targets,
            ui_bindings,
            api_bindings,
        })
    }

    /// 新发送开始时清除旧回执并冻结命令和目标，校验 owner/generation 后才允许进入服务发送流程。
    ///
    /// @author kongweiguang
    fn send_workspace_broadcast_command(&mut self, tab_id: &TabId, cx: &mut Context<Self>) {
        if self.workspace_broadcast.active_command.is_some() {
            self.report_workspace_broadcast_busy(tab_id);
            return;
        }
        let Some((draft_snapshot, command)) = self
            .workspace_broadcast
            .tabs
            .get(tab_id)
            .map(|state| (state.draft.clone(), state.draft.trim().to_owned()))
            .filter(|(_, command)| !command.is_empty())
        else {
            return;
        };
        let Some((layout, _)) = self.workspace_controller.as_ref().and_then(|controller| {
            self.workspace_broadcast_view_layout(controller.snapshot(), tab_id, cx)
        }) else {
            return;
        };
        if let Some(error) = broadcast_draft_error(&draft_snapshot) {
            if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
                state.set_status(error.to_string(), true);
            }
            return;
        }
        let bindings = self.selected_workspace_bindings(tab_id, &layout);
        let Some(request) = self.freeze_workspace_broadcast_request(
            tab_id,
            command.clone(),
            draft_snapshot,
            bindings.clone(),
            cx,
        ) else {
            if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
                state.set_status("所选目标已不可用，请重新选择后再发送。", true);
            }
            return;
        };
        let request_id = self.next_workspace_broadcast_request_id();
        let cancellation = CancellationToken::new();
        self.workspace_broadcast.active_command = Some(ActiveWorkspaceCommand {
            request_id,
            tab_id: tab_id.clone(),
            cancellation: cancellation.clone(),
            request: request.clone(),
        });
        if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
            state.begin_command(request_id, "正在校验所选终端…");
        }
        let sessions = self.services.sessions.clone();
        let executor = self.services.executor.clone();
        let validation_bindings = bindings;
        let validation_cancellation = cancellation.clone();
        let callback_request = request;
        let callback_tab = tab_id.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |operation_cancellation| async move {
                    let validation = terminal_tool_actions::validate_broadcast_start(
                        sessions.as_ref(),
                        validation_bindings,
                        || {
                            validation_cancellation.is_cancelled()
                                || operation_cancellation.is_cancelled()
                        },
                    )
                    .await?;
                    if validation.checks.iter().any(|check| {
                        check.availability
                            != terminal_tool_actions::BroadcastTargetAvailability::Ready
                    }) || !validation.binding_failures.is_empty()
                    {
                        return Err(kerminal_application::AppError::conflict(
                            "一个或多个终端目标已变化或不可用",
                        ));
                    }
                    Ok(())
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if !root
                    .workspace_broadcast
                    .active_command
                    .as_ref()
                    .is_some_and(|active| {
                        active.request_id == request_id && active.tab_id == callback_tab
                    })
                {
                    return;
                }
                let cancelled = root
                    .workspace_broadcast
                    .active_command
                    .as_ref()
                    .is_some_and(|active| active.cancellation.is_cancelled());
                root.workspace_broadcast.active_command = None;
                let current = root
                    .workspace_broadcast
                    .tabs
                    .get(&callback_tab)
                    .is_some_and(|state| state.command_request_id == Some(request_id));
                if !current {
                    return;
                }
                if cancelled {
                    if let Some(state) = root.workspace_broadcast.tabs.get_mut(&callback_tab) {
                        state.set_status("发送校验已取消。", true);
                    }
                    cx.notify();
                    return;
                }
                match result {
                    Ok(()) => {
                        if !root.workspace_broadcast_request_bindings_are_current(&callback_request)
                        {
                            if let Some(state) =
                                root.workspace_broadcast.tabs.get_mut(&callback_tab)
                            {
                                state.set_status("发送目标已变化，请重新选择后重试。", true);
                            }
                        } else if root
                            .workspace_broadcast
                            .tabs
                            .get(&callback_tab)
                            .is_none_or(|state| state.draft.trim() != callback_request.command)
                        {
                            if let Some(state) =
                                root.workspace_broadcast.tabs.get_mut(&callback_tab)
                            {
                                state.set_status("草稿已变化，请重新发送当前内容。", true);
                            }
                        } else {
                            root.execute_workspace_broadcast_request(callback_request, cx);
                        }
                    }
                    Err(error) => {
                        if let Some(state) = root.workspace_broadcast.tabs.get_mut(&callback_tab) {
                            state.set_status(error.public_message(), true);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 校验通过后按冻结目标顺序发送，并保留每个目标的确定性回执与取消状态。
    ///
    /// @author kongweiguang
    fn execute_workspace_broadcast_request(
        &mut self,
        request: FrozenBroadcastRequest,
        cx: &mut Context<Self>,
    ) {
        if self.workspace_broadcast.active_command.is_some() {
            self.report_workspace_broadcast_busy(&request.tab_id);
            return;
        }
        if !self.workspace_broadcast_request_bindings_are_current(&request) {
            if let Some(state) = self.workspace_broadcast.tabs.get_mut(&request.tab_id) {
                state.set_status("发送目标已变化，请重新选择后重试。", true);
            }
            cx.notify();
            return;
        }
        let request_id = self.next_workspace_broadcast_request_id();
        let tab_id = request.tab_id.clone();
        let cancellation = CancellationToken::new();
        self.workspace_broadcast.active_command = Some(ActiveWorkspaceCommand {
            request_id,
            tab_id: tab_id.clone(),
            cancellation: cancellation.clone(),
            request: request.clone(),
        });
        if let Some(state) = self.workspace_broadcast.tabs.get_mut(&tab_id) {
            state.begin_command(request_id, "正在向所选终端发送…");
        }
        let Some(commands) = self.services.commands.clone() else {
            self.workspace_broadcast.active_command = None;
            if let Some(state) = self.workspace_broadcast.tabs.get_mut(&tab_id) {
                state.set_status("命令服务当前不可用。", true);
            }
            return;
        };
        let executor = self.services.executor.clone();
        let user_cancellation = cancellation.clone();
        let execute_request = request.clone();
        let callback_request = request;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |application_cancellation| async move {
                    commands
                        .broadcast_with_cancellation(
                            execute_request.command,
                            execute_request.targets,
                            execute_request.api_bindings,
                            true,
                            user_cancellation,
                            application_cancellation,
                        )
                        .await
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if !root
                    .workspace_broadcast
                    .active_command
                    .as_ref()
                    .is_some_and(|active| {
                        active.request_id == request_id && active.tab_id == tab_id
                    })
                {
                    return;
                }
                root.workspace_broadcast.active_command = None;
                let Some(state) = root.workspace_broadcast.tabs.get_mut(&tab_id) else {
                    return;
                };
                if state.command_request_id != Some(request_id) {
                    return;
                }
                match result {
                    Ok(result) => {
                        let deliveries = result.deliveries;
                        let all_sent =
                            result_all_targets_sent(&callback_request.targets, &deliveries);
                        let result = WorkspaceBroadcastResult {
                            request: callback_request.clone(),
                            deliveries,
                        };
                        state.clear_status();
                        if should_clear_workspace_broadcast_draft(
                            all_sent,
                            &state.draft,
                            &callback_request.draft_snapshot,
                        ) {
                            state.draft.clear();
                        }
                        state.last_result = Some(result);
                    }
                    Err(error) => {
                        finish_workspace_broadcast_error(
                            state,
                            callback_request.clone(),
                            error.public_message(),
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 默认关闭同步路由；启动时复用既有 owner/generation 校验并等待 UI 路由激活成功。
    ///
    /// @author kongweiguang
    fn toggle_workspace_broadcast_sync(&mut self, tab_id: &TabId, cx: &mut Context<Self>) {
        if self
            .workspace_broadcast
            .tabs
            .get(tab_id)
            .is_some_and(|state| state.sync_enabled)
        {
            let _ = self.stop_broadcast();
            if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
                state.set_status("同步按键已关闭。", false);
            }
            return;
        }
        if self.workspace_broadcast.sync_validation.is_some() {
            self.cancel_pending_sync_validation();
            let _ = self.stop_broadcast();
            return;
        }
        if self.broadcast_session_ids.is_some() || self.pending_broadcast_generation.is_some() {
            let _ = self.stop_broadcast();
        }
        let Some((layout, _)) = self.workspace_controller.as_ref().and_then(|controller| {
            self.workspace_broadcast_view_layout(controller.snapshot(), tab_id, cx)
        }) else {
            return;
        };
        let bindings = self.selected_workspace_bindings(tab_id, &layout);
        if bindings.len() < 2 {
            if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
                state.set_status("同步按键至少需要两个可输入目标。", true);
            }
            return;
        }
        let session_ids = bindings
            .iter()
            .map(|binding| binding.session_id.clone())
            .collect::<Vec<_>>();
        let captured = match terminal_tool_actions::capture_broadcast_bindings(self, &session_ids) {
            Ok(captured) if captured == bindings => captured,
            _ => {
                if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
                    state.set_status("同步目标绑定已变化，请重新选择。", true);
                }
                return;
            }
        };
        let generation = self.begin_broadcast_start();
        let cancellation = CancellationToken::new();
        self.workspace_broadcast.sync_validation = Some(PendingSyncValidation {
            generation,
            tab_id: tab_id.clone(),
            bindings: captured.clone(),
            cancellation: cancellation.clone(),
        });
        if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
            state.set_status("正在校验同步目标…", false);
        }
        let sessions = self.services.sessions.clone();
        let executor = self.services.executor.clone();
        let callback_tab = tab_id.clone();
        cx.spawn(async move |this, cx| {
            let validation_cancellation = cancellation.clone();
            let result = executor
                .run_cancellable(cancellation, move |operation_cancellation| async move {
                    terminal_tool_actions::validate_broadcast_start(
                        sessions.as_ref(),
                        captured,
                        || {
                            validation_cancellation.is_cancelled()
                                || operation_cancellation.is_cancelled()
                        },
                    )
                    .await
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                let validation_bar_visible = root
                    .workspace_broadcast
                    .tabs
                    .get(&callback_tab)
                    .is_some_and(|state| state.bar_visible)
                    && root
                        .workspace_controller
                        .as_ref()
                        .and_then(|controller| {
                            root.workspace_broadcast_view_layout(
                                controller.snapshot(),
                                &callback_tab,
                                cx,
                            )
                        })
                        .is_some_and(|(layout, _)| layout.pane_ids().len() >= 2);
                let active_tab = (root.center_panel == CenterPanel::Terminal)
                    .then(|| {
                        root.workspace_controller
                            .as_ref()
                            .and_then(|controller| controller.snapshot().active_tab_id.clone())
                    })
                    .flatten();
                let pending_is_current = root
                    .workspace_broadcast
                    .sync_validation
                    .as_ref()
                    .is_some_and(|pending| {
                        pending.generation == generation && pending.tab_id == callback_tab
                    });
                if !pending_is_current {
                    return;
                }
                let pending_bindings_current = root
                    .workspace_broadcast
                    .sync_validation
                    .as_ref()
                    .and_then(|pending| {
                        root.workspace_controller
                            .as_ref()
                            .map(|controller| controller.snapshot())
                            .and_then(|snapshot| {
                                root.workspace_broadcast_committed_bindings(&callback_tab, snapshot)
                            })
                            .map(|selected| {
                                workspace_broadcast_bindings_match_selection(
                                    &pending.bindings,
                                    &selected,
                                )
                            })
                    })
                    .unwrap_or(false);
                if should_cancel_pending_sync_validation(
                    active_tab.as_ref(),
                    &callback_tab,
                    validation_bar_visible,
                    pending_bindings_current,
                ) || !root.broadcast_start_is_current(generation)
                {
                    root.reject_pending_workspace_broadcast_sync(
                        generation,
                        "同步目标、标签或群发栏已变化，未开启同步按键。",
                    );
                    cx.notify();
                    return;
                }
                root.workspace_broadcast.sync_validation = None;
                match result {
                    Ok(validation)
                        if validation.checks.iter().all(|check| {
                            check.availability
                                == terminal_tool_actions::BroadcastTargetAvailability::Ready
                        }) && validation.binding_failures.is_empty()
                            && terminal_tool_actions::stale_registered_broadcast_targets(
                                root,
                                &validation.bindings,
                            )
                            .is_empty()
                            && root
                                .workspace_controller
                                .as_ref()
                                .map(|controller| controller.snapshot())
                                .and_then(|snapshot| {
                                    root.workspace_broadcast_committed_bindings(
                                        &callback_tab,
                                        snapshot,
                                    )
                                })
                                .is_some_and(|selected| {
                                    workspace_broadcast_bindings_match_selection(
                                        &validation.bindings,
                                        &selected,
                                    )
                                }) =>
                    {
                        if root.activate_broadcast(generation, validation.bindings.clone()) {
                            if let Some(state) =
                                root.workspace_broadcast.tabs.get_mut(&callback_tab)
                            {
                                state.sync_enabled = true;
                                state.set_status("同步按键已开启。", false);
                            }
                        } else {
                            let _ = root.stop_broadcast();
                            if let Some(state) =
                                root.workspace_broadcast.tabs.get_mut(&callback_tab)
                            {
                                state.set_status("同步目标已变化，未开启同步按键。", true);
                            }
                        }
                    }
                    Ok(_) => {
                        let _ = root.stop_broadcast();
                        if let Some(state) = root.workspace_broadcast.tabs.get_mut(&callback_tab) {
                            state.set_status("同步目标不可用或已变化，未开启同步按键。", true);
                        }
                    }
                    Err(error) => {
                        let _ = root.stop_broadcast();
                        if let Some(state) = root.workspace_broadcast.tabs.get_mut(&callback_tab) {
                            state.set_status(error.public_message(), true);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 只有仍然注册在同一个 pane 的 UI owner 才能继续用于校验后的发送。
    ///
    /// @author kongweiguang
    fn workspace_broadcast_request_bindings_are_current(
        &self,
        request: &FrozenBroadcastRequest,
    ) -> bool {
        !request.ui_bindings.is_empty()
            && request.ui_bindings.iter().all(|binding| {
                let SessionOwner::Ui { tab_id, pane_id } = &binding.owner else {
                    return false;
                };
                tab_id == &request.tab_id
                    && self.workspace_broadcast_input_binding(tab_id, pane_id)
                        == Some(binding.clone())
                    && self.workspace_runtime.pane(pane_id).is_some_and(|pane| {
                        let Some(current_target) = pane.target.as_ref() else {
                            return false;
                        };
                        request
                            .targets
                            .iter()
                            .find(|target| target.pane_id == *pane_id)
                            .is_some_and(|target| {
                                history_target_for_session(current_target) == target.target
                            })
                    })
            })
    }

    /// 同步目标的校验请求被 Stop、切 Tab 或生命周期变化撤销时，令它的异步回执失效。
    ///
    /// @author kongweiguang
    fn cancel_pending_sync_validation(&mut self) {
        if let Some(pending) = self.workspace_broadcast.sync_validation.take() {
            let state = self.workspace_broadcast.tabs.get_mut(&pending.tab_id);
            pending.cancel(state);
        }
    }

    /// 取消指定 Tab 的广播执行；应用服务保留已完成目标的结果并停止继续派发。
    ///
    /// @author kongweiguang
    fn cancel_active_workspace_command_for_tab(&mut self, tab_id: Option<&TabId>, message: &str) {
        let Some(active) = self.workspace_broadcast.active_command.as_ref() else {
            return;
        };
        if tab_id.is_some_and(|tab_id| active.tab_id != *tab_id) {
            return;
        }
        active.cancellation.cancel();
        if let Some(state) = self.workspace_broadcast.tabs.get_mut(&active.tab_id) {
            state.set_status(message, true);
        }
    }

    /// 请求代次单调递增，防止迟到回执覆盖较新的草稿或请求状态。
    fn next_workspace_broadcast_request_id(&mut self) -> u64 {
        self.workspace_broadcast.next_request_id = self
            .workspace_broadcast
            .next_request_id
            .wrapping_add(1)
            .max(1);
        self.workspace_broadcast.next_request_id
    }
}

impl Drop for WorkspaceBroadcastRuntime {
    /// Root 实体销毁时同步撤销未完成的校验和发送，避免后台任务保留陈旧 UI 目标。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        if let Some(pending) = self.sync_validation.take() {
            pending.cancellation.cancel();
        }
        if let Some(active) = self.active_command.as_ref() {
            active.cancellation.cancel();
        }
    }
}

/// 同一目标枚举映射给历史 DTO 与 CommandsService，避免分屏功能复制目标含义。
///
/// @author kongweiguang
fn history_target_for_session(target: &SessionTarget) -> CommandHistoryTarget {
    match target {
        SessionTarget::Local(_) => CommandHistoryTarget::Local,
        SessionTarget::Ssh(_) => CommandHistoryTarget::Ssh,
        SessionTarget::Telnet(_) => CommandHistoryTarget::Telnet,
        SessionTarget::Serial(_) => CommandHistoryTarget::Serial,
        SessionTarget::Container(_) => CommandHistoryTarget::Container,
    }
}

/// 逐目标回执区分确定送达、未尝试取消和已经尝试但状态未知的写入。
///
/// @author kongweiguang
fn delivery_state_label(delivery: &BroadcastDeliveryResult) -> String {
    if delivery.sent {
        if delivery.history_error_code.is_some() {
            "已发送，历史记录未确认".to_owned()
        } else {
            "已发送".to_owned()
        }
    } else if delivery.attempted {
        format!("写入结果未知 ({:?})", delivery.error_code)
    } else if delivery.cancelled {
        "尚未发送，已取消".to_owned()
    } else {
        "尚未发送".to_owned()
    }
}

/// 只把唯一匹配的回执当作目标结果，重复回执对该目标保持未知状态。
///
/// @author kongweiguang
fn unique_broadcast_delivery<'a>(
    target: &BroadcastTarget,
    deliveries: &'a [BroadcastDeliveryResult],
) -> Option<&'a BroadcastDeliveryResult> {
    let mut matching = deliveries
        .iter()
        .filter(|delivery| delivery.pane_id == target.pane_id);
    let delivery = matching.next()?;
    matching.next().is_none().then_some(delivery)
}

/// 结果明细始终按发送时冻结的目标顺序生成；不完整或重复回执显示为未确认。
///
/// @author kongweiguang
fn result_delivery_details(
    result: &WorkspaceBroadcastResult,
) -> Vec<WorkspaceBroadcastDeliveryProjection> {
    result
        .request
        .targets
        .iter()
        .filter_map(|target| {
            let delivery = unique_broadcast_delivery(target, &result.deliveries);
            if delivery
                .is_some_and(|delivery| delivery.sent && delivery.history_error_code.is_none())
            {
                return None;
            }
            let state = delivery.map(delivery_state_label).unwrap_or_else(|| {
                crate::locale::text("结果未确认", "Outcome unconfirmed").to_string()
            });
            Some(WorkspaceBroadcastDeliveryProjection {
                title: target.title.clone(),
                state,
            })
        })
        .collect()
}

/// 只有每个冻结目标恰有一条成功回执且没有外来回执时才允许清空草稿。
///
/// @author kongweiguang
fn result_all_targets_sent(
    targets: &[BroadcastTarget],
    deliveries: &[BroadcastDeliveryResult],
) -> bool {
    !targets.is_empty()
        && deliveries.len() == targets.len()
        && targets.iter().all(|target| {
            unique_broadcast_delivery(target, deliveries).is_some_and(|delivery| delivery.sent)
        })
        && deliveries.iter().all(|delivery| {
            targets
                .iter()
                .any(|target| target.pane_id == delivery.pane_id)
        })
}

/// 按布局顺序应用全选、当前和自选模式；是否可输入仍由调用方的 exact binding 过滤。
///
/// @author kongweiguang
fn select_workspace_broadcast_panes(
    mode: WorkspaceBroadcastTargetMode,
    custom_panes: &BTreeSet<PaneId>,
    layout_panes: &[PaneId],
    focused_pane: Option<&PaneId>,
) -> Vec<PaneId> {
    layout_panes
        .iter()
        .filter(|pane_id| match mode {
            WorkspaceBroadcastTargetMode::All => true,
            WorkspaceBroadcastTargetMode::Current => focused_pane == Some(*pane_id),
            WorkspaceBroadcastTargetMode::Custom => custom_panes.contains(*pane_id),
        })
        .cloned()
        .collect()
}

/// 比较完整 binding 集合而不依赖布局顺序，禁止 All 扩容后沿用旧的较小路由。
///
/// @author kongweiguang
fn workspace_broadcast_bindings_match_selection(
    active: &[UiSessionBinding],
    selected: &[UiSessionBinding],
) -> bool {
    active.len() == selected.len()
        && active.iter().all(|binding| selected.contains(binding))
        && selected.iter().all(|binding| active.contains(binding))
}

/// 按候选布局更新自动展开边界并移除已关闭 pane，保留其余用户显式设置。
///
/// @author kongweiguang
fn reconcile_workspace_broadcast_tab_state(
    state: &mut WorkspaceBroadcastTabState,
    view_panes: &[PaneId],
) {
    let previous_count = state.previous_view_pane_count;
    let pane_count = view_panes.len();
    if pane_count >= 2 && previous_count < 2 {
        state.bar_visible = true;
    }
    let current_panes = view_panes.iter().cloned().collect::<BTreeSet<_>>();
    state
        .custom_panes
        .retain(|pane_id| current_panes.contains(pane_id));
    state.previous_view_pane_count = pane_count;
}

/// Tab 改变、工具栏收起或目标绑定失效时取消命令的后续目标。
///
/// @author kongweiguang
fn should_cancel_active_workspace_command(
    active_tab: Option<&TabId>,
    command_tab: &TabId,
    bar_visible: bool,
    bindings_current: bool,
) -> bool {
    active_tab != Some(command_tab) || !bar_visible || !bindings_current
}

/// 同步启动的迟到校验只在原 Tab 仍活动且群发栏仍可见时有效。
///
/// @author kongweiguang
fn should_cancel_pending_sync_validation(
    active_tab: Option<&TabId>,
    validation_tab: &TabId,
    bar_visible: bool,
    bindings_current: bool,
) -> bool {
    active_tab != Some(validation_tab) || !bar_visible || !bindings_current
}

/// 只有全部冻结目标成功且草稿仍保持发送时快照，回执才能清空当前输入。
///
/// @author kongweiguang
fn should_clear_workspace_broadcast_draft(
    all_targets_sent: bool,
    current_draft: &str,
    sent_draft_snapshot: &str,
) -> bool {
    all_targets_sent && current_draft == sent_draft_snapshot
}

#[cfg(test)]
#[path = "../../tests/unit/root_workspace_broadcast.rs"]
mod tests;
