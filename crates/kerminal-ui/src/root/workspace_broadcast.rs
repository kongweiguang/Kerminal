//! Root owns per-Tab broadcast policy and connects the shared toolbar to existing session services.
//!
//! @author kongweiguang

use std::collections::{BTreeSet, HashMap};

use gpui_kit::{AnyElement, AppContext as _, Context, Entity, IntoElement, Subscription, Window};
use kerminal_application::{
    commands::{
        BroadcastDeliveryResult, BroadcastSessionBinding, SnippetRenderResult,
        direct_snippet_catalog_command,
    },
    sessions::UiSessionBinding,
    workspace::{WorkspaceSessionSnapshot, WorkspaceSessionTab},
};
use kerminal_domain::{
    commands::{
        BroadcastAnalysis, BroadcastTarget, CommandHistoryTarget, SnippetCatalogListRequest,
        SnippetCatalogOrigin, SnippetPreferenceKey,
    },
    identity::{PaneId, TabId},
    sessions::{SessionOwner, SessionTarget},
    workspace::TerminalLayoutNode,
};
use kerminal_terminal::input::{
    InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyEvent,
};
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;

use crate::{
    features::{
        workspace_broadcast::{
            PaneBroadcastProjection, WorkspaceBroadcastBar, WorkspaceBroadcastBarEvent,
            WorkspaceBroadcastBarProjection, WorkspaceBroadcastDeliveryProjection,
            WorkspaceBroadcastSnippetProjection, WorkspaceBroadcastTargetMode,
            WorkspaceBroadcastTargetProjection,
        },
        workspace_panes::WorkspacePaneEvent,
    },
    root::{CenterPanel, RootView, session_queue, terminal_tool_actions},
};

const MAX_BROADCAST_COMMAND_BYTES: usize = 16 * 1024;

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
    results_open: bool,
    command_request_id: Option<u64>,
    confirmation: Option<FrozenBroadcastRequest>,
    last_result: Option<WorkspaceBroadcastResult>,
    snippets_open: bool,
    snippets: Vec<SnippetChoice>,
    snippet_generation: u64,
}

#[derive(Clone)]
struct SnippetChoice {
    key: SnippetPreferenceKey,
    title: String,
    command: SnippetRenderResult,
}

#[derive(Clone)]
struct FrozenBroadcastRequest {
    tab_id: TabId,
    command: String,
    draft_snapshot: String,
    targets: Vec<BroadcastTarget>,
    ui_bindings: Vec<UiSessionBinding>,
    api_bindings: Vec<BroadcastSessionBinding>,
    analysis: BroadcastAnalysis,
    snippet: Option<SnippetRenderResult>,
}

struct BroadcastPlan {
    execute: FrozenBroadcastRequest,
    full: FrozenBroadcastRequest,
    prior_deliveries: Vec<BroadcastDeliveryResult>,
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
    fn default() -> Self {
        Self {
            mode: WorkspaceBroadcastTargetMode::All,
            custom_panes: BTreeSet::new(),
            draft: String::new(),
            bar_visible: true,
            previous_view_pane_count: 0,
            sync_enabled: false,
            status: None,
            results_open: false,
            command_request_id: None,
            confirmation: None,
            last_result: None,
            snippets_open: false,
            snippets: Vec::new(),
            snippet_generation: 0,
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
            if let Some(previous_tab_id) = self.workspace_broadcast.last_active_tab_id.as_ref()
                && let Some(state) = self.workspace_broadcast.tabs.get_mut(previous_tab_id)
            {
                invalidate_snippet_picker(state);
            }
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
                state.status = Some("群发目标连接或选择已变化，同步输入已停止。".to_owned());
            }
        }
        self.sync_bar_projection(&snapshot, active_tab.as_ref(), _window, cx);
    }

    /// 首次绘制前复用候选布局决定工具栏显隐，保证测量时已包含其占用空间。
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
                    state.status = None;
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

    /// 菜单与页头共用精确绑定下的选中集合，标题只供阅读，不参与目标路由。
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
            confirmation_required: state.confirmation.is_some(),
            can_cancel: self
                .workspace_broadcast
                .active_command
                .as_ref()
                .is_some_and(|active| active.tab_id == *tab_id),
            execution_busy: self.workspace_broadcast.active_command.is_some(),
            can_retry_safe: state
                .last_result
                .as_ref()
                .is_some_and(WorkspaceBroadcastResult::has_safe_retry),
            result_details: state
                .last_result
                .as_ref()
                .map(|result| {
                    result
                        .deliveries
                        .iter()
                        .map(|delivery| {
                            let title = result
                                .request
                                .targets
                                .iter()
                                .find(|target| target.pane_id == delivery.pane_id)
                                .map(|target| target.title.clone())
                                .unwrap_or_else(|| delivery.pane_id.to_string());
                            WorkspaceBroadcastDeliveryProjection {
                                title,
                                state: delivery_state_label(delivery),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default(),
            results_open: state.results_open,
            snippets_open: state.snippets_open,
            snippets: state
                .snippets
                .iter()
                .map(|snippet| WorkspaceBroadcastSnippetProjection {
                    id: snippet_choice_id(&snippet.key),
                    title: snippet.title.clone(),
                })
                .collect(),
            status: state.status.clone(),
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
        state.confirmation = None;
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
                state.status = Some(message.to_owned());
            }
        }
    }

    /// 忙碌提示保留每个 Tab 的确认和失败结果，等待当前逐目标回执收敛后再操作。
    ///
    /// @author kongweiguang
    fn report_workspace_broadcast_busy(&mut self, tab_id: &TabId) {
        if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
            state.status = Some("另一项群发仍在收尾，请稍后重试。".to_owned());
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
            state.status = Some(message.to_owned());
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

    /// 工具栏事件先校验捕获的 Tab，再更新状态或启动异步操作。
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
            | WorkspaceBroadcastBarEvent::SendCommand { tab_id }
            | WorkspaceBroadcastBarEvent::ConfirmSend { tab_id }
            | WorkspaceBroadcastBarEvent::Interrupt { tab_id }
            | WorkspaceBroadcastBarEvent::CancelCommand { tab_id }
            | WorkspaceBroadcastBarEvent::RetrySafeFailures { tab_id }
            | WorkspaceBroadcastBarEvent::ToggleResults { tab_id }
            | WorkspaceBroadcastBarEvent::OpenSnippets { tab_id }
            | WorkspaceBroadcastBarEvent::CloseSnippets { tab_id }
            | WorkspaceBroadcastBarEvent::ChooseSnippet { tab_id, .. } => tab_id.clone(),
        };
        if self.workspace_broadcast.last_active_tab_id.as_ref() != Some(&tab_id) {
            return;
        }
        match event {
            WorkspaceBroadcastBarEvent::DraftChanged { draft, .. } => {
                if draft.len() <= MAX_BROADCAST_COMMAND_BYTES
                    && let Some(state) = self.workspace_broadcast.tabs.get_mut(&tab_id)
                {
                    state.draft = draft;
                    state.confirmation = None;
                    state.status = None;
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
            WorkspaceBroadcastBarEvent::ConfirmSend { .. } => {
                if self.workspace_broadcast.active_command.is_some() {
                    self.report_workspace_broadcast_busy(&tab_id);
                } else if let Some(request) = self
                    .workspace_broadcast
                    .tabs
                    .get_mut(&tab_id)
                    .and_then(|state| state.confirmation.take())
                {
                    let draft_matches = self
                        .workspace_broadcast
                        .tabs
                        .get(&tab_id)
                        .is_some_and(|state| state.draft.trim() == request.command);
                    if draft_matches {
                        self.execute_workspace_broadcast_plan(
                            BroadcastPlan {
                                full: request.clone(),
                                execute: request,
                                prior_deliveries: Vec::new(),
                            },
                            true,
                            cx,
                        );
                    } else if let Some(state) = self.workspace_broadcast.tabs.get_mut(&tab_id) {
                        state.status = Some("草稿已变化，请重新发送当前内容。".to_owned());
                    }
                }
            }
            WorkspaceBroadcastBarEvent::Interrupt { .. } => {
                self.interrupt_workspace_broadcast_targets(&tab_id, cx);
            }
            WorkspaceBroadcastBarEvent::CancelCommand { .. } => {
                if let Some(active) = self.workspace_broadcast.active_command.as_ref()
                    && active.tab_id == tab_id
                {
                    active.cancellation.cancel();
                    if let Some(state) = self.workspace_broadcast.tabs.get_mut(&tab_id) {
                        state.status = Some("正在取消后续目标；当前写入完成后收尾。".to_owned());
                    }
                }
            }
            WorkspaceBroadcastBarEvent::RetrySafeFailures { .. } => {
                if self.workspace_broadcast.active_command.is_some() {
                    self.report_workspace_broadcast_busy(&tab_id);
                } else {
                    self.retry_safe_workspace_broadcast_targets(&tab_id, cx);
                }
            }
            WorkspaceBroadcastBarEvent::ToggleResults { .. } => {
                if let Some(state) = self.workspace_broadcast.tabs.get_mut(&tab_id) {
                    state.results_open = !state.results_open;
                }
            }
            WorkspaceBroadcastBarEvent::OpenSnippets { .. } => {
                self.open_workspace_broadcast_snippets(&tab_id, cx);
            }
            WorkspaceBroadcastBarEvent::CloseSnippets { .. } => {
                if let Some(state) = self.workspace_broadcast.tabs.get_mut(&tab_id) {
                    invalidate_snippet_picker(state);
                }
            }
            WorkspaceBroadcastBarEvent::ChooseSnippet { snippet_id, .. } => {
                self.choose_workspace_broadcast_snippet(&tab_id, &snippet_id, cx);
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
        state.confirmation = None;
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
            analysis,
            snippet: None,
        })
    }

    /// 打开片段列表只读取既有目录服务，加载回执以 per-Tab 代次防止覆盖更新状态。
    ///
    /// @author kongweiguang
    fn open_workspace_broadcast_snippets(&mut self, tab_id: &TabId, cx: &mut Context<Self>) {
        let Some(commands) = self.services.commands.clone() else {
            if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
                state.status = Some("命令片段服务当前不可用。".to_owned());
            }
            return;
        };
        let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) else {
            return;
        };
        state.snippets_open = true;
        state.snippet_generation = state.snippet_generation.wrapping_add(1).max(1);
        let generation = state.snippet_generation;
        state.status = Some("正在读取命令片段…".to_owned());
        let executor = self.services.executor.clone();
        let callback_tab = tab_id.clone();
        let request = SnippetCatalogListRequest {
            query: None,
            origin: None,
            scope: None,
            limit: None,
        };
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_cancellation| async move { commands.list_snippets(request).await })
                .await;
            let _ = this.update(cx, |root, cx| {
                let Some(state) = root.workspace_broadcast.tabs.get_mut(&callback_tab) else {
                    return;
                };
                if !is_current_snippet_request(state, generation) {
                    return;
                }
                match result {
                    Ok(items) => {
                        state.snippets = items
                            .into_iter()
                            .map(|item| SnippetChoice {
                                command: direct_snippet_catalog_command(&item, &BTreeMap::new()),
                                key: SnippetPreferenceKey {
                                    origin: item.origin,
                                    snippet_id: item.id,
                                },
                                title: item.title,
                            })
                            .collect();
                        state.status = if state.snippets.is_empty() {
                            Some("当前没有可用的命令片段。".to_owned())
                        } else {
                            None
                        };
                    }
                    Err(error) => state.status = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 点击即发送当前展示的片段快照；关闭弹层只影响目录，不撤销已明确发起的发送。
    /// 目标仍固定为点击时的完整 binding，命令不覆盖输入草稿，也不依赖平台探测或 Shell marker。
    ///
    /// @author kongweiguang
    fn choose_workspace_broadcast_snippet(
        &mut self,
        tab_id: &TabId,
        choice_id: &str,
        cx: &mut Context<Self>,
    ) {
        if self.workspace_broadcast.active_command.is_some() {
            self.report_workspace_broadcast_busy(tab_id);
            return;
        }
        let Some(choice) = self.workspace_broadcast.tabs.get(tab_id).and_then(|state| {
            state
                .snippets
                .iter()
                .find(|snippet| snippet_choice_id(&snippet.key) == choice_id)
                .cloned()
        }) else {
            return;
        };
        let Some((layout, _)) = self.workspace_controller.as_ref().and_then(|controller| {
            self.workspace_broadcast_view_layout(controller.snapshot(), tab_id, cx)
        }) else {
            return;
        };
        let bindings = self.selected_workspace_bindings(tab_id, &layout);
        let draft_snapshot = self
            .workspace_broadcast
            .tabs
            .get(tab_id)
            .map(|state| state.draft.clone())
            .unwrap_or_default();
        let Some(mut request) = self.freeze_workspace_broadcast_request(
            tab_id,
            choice.command.command.clone(),
            draft_snapshot,
            bindings,
            cx,
        ) else {
            if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
                state.status = Some("请选择仍可输入的分屏后发送片段。".to_owned());
            }
            return;
        };
        request.snippet = Some(choice.command);
        if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
            invalidate_snippet_picker(state);
            state.confirmation = None;
            state.last_result = None;
            state.results_open = false;
        }
        self.execute_workspace_broadcast_plan(
            BroadcastPlan {
                full: request.clone(),
                execute: request,
                prior_deliveries: Vec::new(),
            },
            true,
            cx,
        );
    }

    /// 发送时冻结命令和目标集合；只有 CommandsService 的风险门禁要求时才二次确认。
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
            .filter(|(_, command)| {
                !command.is_empty() && command.len() <= MAX_BROADCAST_COMMAND_BYTES
            })
        else {
            return;
        };
        let Some((layout, _)) = self.workspace_controller.as_ref().and_then(|controller| {
            self.workspace_broadcast_view_layout(controller.snapshot(), tab_id, cx)
        }) else {
            return;
        };
        let bindings = self.selected_workspace_bindings(tab_id, &layout);
        let Some(request) = self.freeze_workspace_broadcast_request(
            tab_id,
            command.clone(),
            draft_snapshot,
            bindings.clone(),
            cx,
        ) else {
            if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
                state.status = Some("所选目标已不可用，请重新选择后再发送。".to_owned());
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
            state.command_request_id = Some(request_id);
            state.confirmation = None;
            state.last_result = None;
            state.results_open = false;
            state.status = Some("正在校验所选终端…".to_owned());
        }
        let sessions = self.services.sessions.clone();
        let executor = self.services.executor.clone();
        let validation_bindings = bindings;
        let validation_cancellation = cancellation.clone();
        let callback_request = request.clone();
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
                    Ok(callback_request)
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
                        state.status = Some("发送校验已取消。".to_owned());
                    }
                    cx.notify();
                    return;
                }
                match result {
                    Ok(request) => {
                        if !root.workspace_broadcast_request_bindings_are_current(&request) {
                            if let Some(state) =
                                root.workspace_broadcast.tabs.get_mut(&callback_tab)
                            {
                                state.status =
                                    Some("发送目标已变化，请重新选择后重试。".to_owned());
                            }
                        } else if root
                            .workspace_broadcast
                            .tabs
                            .get(&callback_tab)
                            .is_none_or(|state| state.draft.trim() != request.command)
                        {
                            if let Some(state) =
                                root.workspace_broadcast.tabs.get_mut(&callback_tab)
                            {
                                state.status = Some("草稿已变化，请重新发送当前内容。".to_owned());
                            }
                        } else if request.analysis.requires_confirmation {
                            if let Some(state) =
                                root.workspace_broadcast.tabs.get_mut(&callback_tab)
                            {
                                state.confirmation = Some(request);
                                state.status = Some("该命令需要确认后才会发送。".to_owned());
                            }
                        } else {
                            root.execute_workspace_broadcast_plan(
                                BroadcastPlan {
                                    full: request.clone(),
                                    execute: request,
                                    prior_deliveries: Vec::new(),
                                },
                                false,
                                cx,
                            );
                        }
                    }
                    Err(error) => {
                        if let Some(state) = root.workspace_broadcast.tabs.get_mut(&callback_tab) {
                            state.status = Some(error.public_message().to_owned());
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 共享逐目标回执与取消流程；片段直接提交完整 binding，普通草稿保留既有确认策略。
    ///
    /// @author kongweiguang
    fn execute_workspace_broadcast_plan(
        &mut self,
        plan: BroadcastPlan,
        confirmed: bool,
        cx: &mut Context<Self>,
    ) {
        if self.workspace_broadcast.active_command.is_some() {
            self.report_workspace_broadcast_busy(&plan.full.tab_id);
            return;
        }
        if !self.workspace_broadcast_request_bindings_are_current(&plan.execute) {
            if let Some(state) = self.workspace_broadcast.tabs.get_mut(&plan.full.tab_id) {
                state.status = Some("发送目标已变化，请重新选择后重试。".to_owned());
            }
            cx.notify();
            return;
        }
        let request_id = self.next_workspace_broadcast_request_id();
        let tab_id = plan.full.tab_id.clone();
        let cancellation = CancellationToken::new();
        self.workspace_broadcast.active_command = Some(ActiveWorkspaceCommand {
            request_id,
            tab_id: tab_id.clone(),
            cancellation: cancellation.clone(),
            request: plan.execute.clone(),
        });
        if let Some(state) = self.workspace_broadcast.tabs.get_mut(&tab_id) {
            state.command_request_id = Some(request_id);
            state.status = Some("正在向所选终端发送…".to_owned());
        }
        let Some(commands) = self.services.commands.clone() else {
            self.workspace_broadcast.active_command = None;
            if let Some(state) = self.workspace_broadcast.tabs.get_mut(&tab_id) {
                state.status = Some("命令服务当前不可用。".to_owned());
            }
            return;
        };
        let executor = self.services.executor.clone();
        let user_cancellation = cancellation.clone();
        let execute_request = plan.execute.clone();
        let callback_plan = plan;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |application_cancellation| async move {
                    if let Some(snippet) = execute_request.snippet {
                        commands
                            .broadcast_snippet_with_cancellation(
                                snippet,
                                execute_request.targets,
                                execute_request.ui_bindings,
                                user_cancellation,
                                application_cancellation,
                            )
                            .await
                    } else {
                        commands
                            .broadcast_with_cancellation(
                                execute_request.command,
                                execute_request.targets,
                                execute_request.api_bindings,
                                confirmed,
                                user_cancellation,
                                application_cancellation,
                            )
                            .await
                    }
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if !root.workspace_broadcast.active_command.as_ref().is_some_and(|active| {
                    active.request_id == request_id && active.tab_id == tab_id
                }) {
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
                        let mut deliveries = callback_plan.prior_deliveries.clone();
                        for delivery in result.deliveries {
                            if let Some(previous) = deliveries
                                .iter_mut()
                                .find(|previous| previous.pane_id == delivery.pane_id)
                            {
                                *previous = delivery;
                            } else {
                                deliveries.push(delivery);
                            }
                        }
                        deliveries.sort_by_key(|delivery| {
                            callback_plan
                                .full
                                .targets
                                .iter()
                                .position(|target| target.pane_id == delivery.pane_id)
                                .unwrap_or(usize::MAX)
                        });
                        let sent = deliveries.iter().filter(|delivery| delivery.sent).count();
                        let all_sent = result_all_targets_sent(
                            &callback_plan.full.targets,
                            &deliveries,
                        );
                        let attempted_failed = deliveries
                            .iter()
                            .filter(|delivery| delivery.attempted && !delivery.sent)
                            .count();
                        let not_sent = deliveries
                            .iter()
                            .filter(|delivery| !delivery.attempted && !delivery.sent)
                            .count();
                        let result = WorkspaceBroadcastResult {
                            request: callback_plan.full.clone(),
                            deliveries,
                        };
                        state.results_open = attempted_failed > 0 || not_sent > 0;
                        state.status = Some(format!(
                            "已发送 {sent}/{}；结果未知/失败 {attempted_failed}，未发送 {not_sent}。",
                            result.request.targets.len()
                        ));
                        if callback_plan.full.snippet.is_none() && should_clear_workspace_broadcast_draft(
                            all_sent,
                            &state.draft,
                            &callback_plan.full.draft_snapshot,
                        ) {
                            state.draft.clear();
                        }
                        state.confirmation = None;
                        state.last_result = Some(result);
                    }
                    Err(error) => {
                        state.status = Some(error.public_message().to_owned());
                        state.confirmation = None;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 只重试明确未开始写入的取消项；成功或结果未知的目标不会重复执行。
    ///
    /// @author kongweiguang
    fn retry_safe_workspace_broadcast_targets(&mut self, tab_id: &TabId, cx: &mut Context<Self>) {
        let Some(previous) = self
            .workspace_broadcast
            .tabs
            .get(tab_id)
            .and_then(|state| state.last_result.as_ref())
            .filter(|result| result.has_safe_retry())
            .cloned()
        else {
            return;
        };
        let safe_panes = previous
            .deliveries
            .iter()
            .filter(|delivery| is_safe_broadcast_retry(delivery))
            .map(|delivery| delivery.pane_id.clone())
            .collect::<BTreeSet<_>>();
        let mut execute = previous.request.clone();
        execute
            .targets
            .retain(|target| safe_panes.contains(&target.pane_id));
        execute
            .api_bindings
            .retain(|binding| safe_panes.contains(&binding.pane_id));
        execute
            .ui_bindings
            .retain(|binding| matches!(&binding.owner, SessionOwner::Ui { pane_id, .. } if safe_panes.contains(pane_id)));
        if execute.targets.is_empty() || execute.api_bindings.len() != execute.targets.len() {
            return;
        }
        execute.analysis = BroadcastAnalysis::analyze(&execute.command, execute.targets.clone());
        if !self.workspace_broadcast_request_bindings_are_current(&execute) {
            if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
                state.status = Some("原失败目标已变化，未自动换目标重试。".to_owned());
            }
            return;
        }
        self.execute_workspace_broadcast_plan(
            BroadcastPlan {
                execute,
                full: previous.request,
                prior_deliveries: previous.deliveries,
            },
            true,
            cx,
        );
    }

    /// 一次性 Ctrl+C 使用当前选中的精确 binding 进入各会话 FIFO，不借用或改变同步广播状态。
    ///
    /// @author kongweiguang
    fn interrupt_workspace_broadcast_targets(&mut self, tab_id: &TabId, cx: &mut Context<Self>) {
        let Some((layout, _)) = self.workspace_controller.as_ref().and_then(|controller| {
            self.workspace_broadcast_view_layout(controller.snapshot(), tab_id, cx)
        }) else {
            return;
        };
        let bindings = self.selected_workspace_bindings(tab_id, &layout);
        let event = TerminalInputEvent::Key(TerminalKeyEvent {
            key: TerminalKey::Character('c'),
            phase: KeyPhase::Press,
            modifiers: InputModifiers {
                control: true,
                ..InputModifiers::default()
            },
        });
        match self.enqueue_terminal_input_once(&bindings, event, cx) {
            Ok(count) => {
                if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
                    state.status = Some(format!("Ctrl+C 已加入 {count} 个终端的输入队列。"));
                }
            }
            Err(error) => {
                if let Some(state) = self.workspace_broadcast.tabs.get_mut(tab_id) {
                    state.status = Some(format!("Ctrl+C 未发送：{}。", queue_error_label(error)));
                }
            }
        }
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
                state.status = Some("同步按键已关闭。".to_owned());
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
                state.status = Some("同步按键至少需要两个可输入目标。".to_owned());
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
                    state.status = Some("同步目标绑定已变化，请重新选择。".to_owned());
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
            state.status = Some("正在校验同步目标…".to_owned());
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
                                state.status = Some("同步按键已开启。".to_owned());
                            }
                        } else {
                            let _ = root.stop_broadcast();
                            if let Some(state) =
                                root.workspace_broadcast.tabs.get_mut(&callback_tab)
                            {
                                state.status = Some("同步目标已变化，未开启同步按键。".to_owned());
                            }
                        }
                    }
                    Ok(_) => {
                        let _ = root.stop_broadcast();
                        if let Some(state) = root.workspace_broadcast.tabs.get_mut(&callback_tab) {
                            state.status =
                                Some("同步目标不可用或已变化，未开启同步按键。".to_owned());
                        }
                    }
                    Err(error) => {
                        let _ = root.stop_broadcast();
                        if let Some(state) = root.workspace_broadcast.tabs.get_mut(&callback_tab) {
                            state.status = Some(error.public_message().to_owned());
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 只有仍然注册在同一个 pane 的 UI owner 才能继续用于确认、执行或安全重试。
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
            pending.cancellation.cancel();
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
            state.status = Some(message.to_owned());
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

impl WorkspaceBroadcastResult {
    /// 安全重试只包含明确未尝试的取消项；失败写入的状态可能未知，不能重放。
    fn has_safe_retry(&self) -> bool {
        self.deliveries.iter().any(is_safe_broadcast_retry)
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

/// 同一目标枚举映射给历史 DTO 与 CommandsService 的风险分析，避免分屏功能复制目标含义。
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

/// 把来源和片段 ID 合成稳定的 UI 键，防止内置与用户目录同名时选错定义。
///
/// @author kongweiguang
fn snippet_choice_id(key: &SnippetPreferenceKey) -> String {
    let origin = match key.origin {
        SnippetCatalogOrigin::Builtin => "builtin",
        SnippetCatalogOrigin::User => "user",
    };
    format!("{origin}:{}", key.snippet_id)
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

/// 只有每个冻结目标都有明确成功回执时才允许清空草稿。
///
/// @author kongweiguang
fn result_all_targets_sent(
    targets: &[BroadcastTarget],
    deliveries: &[BroadcastDeliveryResult],
) -> bool {
    !targets.is_empty()
        && targets.iter().all(|target| {
            deliveries
                .iter()
                .find(|delivery| delivery.pane_id == target.pane_id)
                .is_some_and(|delivery| delivery.sent)
        })
}

/// 将受限队列拒绝解释成可操作的简短反馈，不向工具栏泄露内部错误细节。
///
/// @author kongweiguang
fn queue_error_label(error: session_queue::SessionQueueError) -> &'static str {
    match error {
        session_queue::SessionQueueError::SessionUnavailable => "目标会话已关闭或发生变化",
        session_queue::SessionQueueError::QueueFull => "终端输入队列已满",
        session_queue::SessionQueueError::BroadcastInactive => "目标路由已停止",
        session_queue::SessionQueueError::BroadcastInputNotSupported => "该输入类型不支持广播",
    }
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
    if pane_count < 2 && previous_count >= 2 {
        invalidate_snippet_picker(state);
    }
    let current_panes = view_panes.iter().cloned().collect::<BTreeSet<_>>();
    state
        .custom_panes
        .retain(|pane_id| current_panes.contains(pane_id));
    state.previous_view_pane_count = pane_count;
}

/// 关闭片段弹层只撤销目录查询回执；已点击发起的发送由独立冻结请求继续收敛。
///
/// @author kongweiguang
fn invalidate_snippet_picker(state: &mut WorkspaceBroadcastTabState) {
    state.snippets_open = false;
    state.snippet_generation = state.snippet_generation.wrapping_add(1).max(1);
}

/// 迟到的片段回执只允许更新仍打开且代次匹配的当前 picker。
///
/// @author kongweiguang
fn is_current_snippet_request(state: &WorkspaceBroadcastTabState, generation: u64) -> bool {
    state.snippets_open && state.snippet_generation == generation
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

/// 只有明确在写入前取消的目标可以安全重试，任何尝试过的写入都视为结果可能未知。
///
/// @author kongweiguang
fn is_safe_broadcast_retry(delivery: &BroadcastDeliveryResult) -> bool {
    !delivery.attempted && delivery.cancelled && !delivery.sent
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
