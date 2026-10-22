//! Root 使用的递归分屏渲染适配器；只消费投影并转发布局意图。
//!
//! @author kongweiguang

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use gpui_kit::{
    AnyElement, AppContext as _, Bounds, ClickEvent, Context, Entity, EventEmitter, IntoElement,
    Pixels, Render, SharedString, Subscription, Window, component::button::Button, div, prelude::*,
    px,
};
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::SessionTarget,
    workspace::TerminalLayoutNode,
};
use kerminal_terminal::snapshot::RenderSnapshot;

use super::session_reconnect::SessionRestartProjection;
use super::session_reconnect::{SessionReconnectProjection, SessionReconnectState};
use super::workspace_runtime::RuntimePanePhase;
use crate::{
    features::host_picker::HostPickerPopoverProjection,
    features::workspace_broadcast::PaneBroadcastProjection,
    features::workspace_controller::WorkspaceResourceOperation,
    features::workspace_panes::{
        PaneHeaderProjection, PaneRefreshProjection, SplitCandidatePreview,
        SplitCandidateProbeContext, WorkspacePaneEvent, WorkspacePaneRefreshEvent, WorkspacePanes,
    },
    theme::ThemeTokens,
};

/// 一片叶子的只读运行态；动作只携带经 service 查询确认的 session binding。
#[derive(Clone)]
pub(super) struct WorkspacePaneProjection {
    pub pane_id: PaneId,
    pub title: SharedString,
    pub session_id: Option<SessionId>,
    pub target: Option<SessionTarget>,
    pub snapshot: Option<RenderSnapshot>,
    pub status: Option<SharedString>,
    pub is_closing: bool,
    /// 仅未准入恢复的候选 pane 显示可取消分屏入口。
    pub split_candidate_cancelable: bool,
    /// ClosePane/CloseTab 已拥有该 pane 时禁止把同一错误状态误呈现为恢复重试。
    pub close_operation_pending: bool,
    pub reconnect: Option<SessionReconnectProjection>,
    /// 当前 pane 的重启 binding 与忙碌状态，按钮在 pane 生命周期内始终占据固定槽位。
    pub restart: Option<SessionRestartProjection>,
    pub broadcast: PaneBroadcastProjection,
}

/// 只向 Root 返回需要业务处理的 pane 意图，不在渲染层创建或关闭会话。
#[derive(Clone, Debug, PartialEq)]
pub(super) enum WorkspaceRenderEvent {
    Pane {
        tab_id: TabId,
        event: WorkspacePaneEvent,
    },
    RetryPane {
        tab_id: TabId,
        pane_id: PaneId,
    },
    ReconnectPane {
        tab_id: TabId,
        pane_id: PaneId,
        binding: UiSessionBinding,
    },
    RefreshPane {
        tab_id: TabId,
        pane_id: PaneId,
        binding: UiSessionBinding,
    },
    CancelReconnect {
        tab_id: TabId,
        pane_id: PaneId,
        binding: UiSessionBinding,
        attempt_id: u64,
    },
    CancelSplitCandidate {
        preview: SplitCandidatePreview,
    },
}

/// 每个叶子的内容由 Root 按明确的 PaneId、SessionId 和快照创建。
type PaneContentRenderer = dyn for<'a> Fn(
        &WorkspacePaneProjection,
        Option<SplitCandidateProbeContext>,
        &mut Window,
        &mut Context<'a, WorkspacePanes>,
    ) -> AnyElement
    + 'static;

/// 将 pane 内重试按钮发送回 WorkspaceRender，而不让叶子访问 Root 服务。
type RetryEmitter = dyn for<'a> Fn(TabId, PaneId, &mut Context<'a, WorkspacePanes>) + 'static;

/// 把显式重连动作返回 Root；视图只携带实时服务查询得到的身份绑定。
type ReconnectEmitter =
    dyn for<'a> Fn(WorkspaceRenderEvent, &mut Context<'a, WorkspacePanes>) + 'static;

/// 候选身份、来源测量与交互锁作为同批只读投影传递，避免更新调用遗漏或串用旧帧。
pub(super) struct SplitCandidateRenderContext {
    pub preview: Option<SplitCandidatePreview>,
    pub source_bounds: Option<(u64, Bounds<Pixels>)>,
    pub interaction_locked: bool,
}

/// 布局、会话和锚定弹层来自同一次 Root 快照，创建与刷新共用整批投影以免遗漏身份。
pub(super) struct WorkspaceRenderProjection {
    pub layout: TerminalLayoutNode,
    pub focused_pane: PaneId,
    pub panes: Vec<WorkspacePaneProjection>,
    pub candidate: SplitCandidateRenderContext,
    pub host_picker: Option<HostPickerPopoverProjection>,
}

/// 将根布局与每 pane 的只读数据接到既有 WorkspacePanes，不复制递归 split 或 resize 逻辑。
pub(super) struct WorkspaceRender {
    tab_id: TabId,
    projections: Rc<RefCell<ProjectionIndex>>,
    host_picker: Rc<RefCell<Option<HostPickerPopoverProjection>>>,
    panes: Entity<WorkspacePanes>,
    _pane_subscription: Subscription,
    _pane_refresh_subscription: Subscription,
}

impl EventEmitter<WorkspaceRenderEvent> for WorkspaceRender {}

impl WorkspaceRender {
    /// 创建稳定的 pane 事件桥；候选上下文按批传递，标题和内容共用持久 pane Entity。
    ///
    /// @author kongweiguang
    pub(super) fn new(
        tab_id: TabId,
        projection: WorkspaceRenderProjection,
        pane_renderer: impl for<'a> Fn(
            &WorkspacePaneProjection,
            Option<SplitCandidateProbeContext>,
            &mut Window,
            &mut Context<'a, WorkspacePanes>,
        ) -> AnyElement
        + 'static,
        cx: &mut Context<Self>,
    ) -> Self {
        let WorkspaceRenderProjection {
            layout,
            focused_pane,
            panes: projections,
            candidate,
            host_picker,
        } = projection;
        let projection_index = Rc::new(RefCell::new(ProjectionIndex::build(
            &layout,
            &focused_pane,
            projections,
        )));
        let host_picker_projection = Rc::new(RefCell::new(host_picker));
        let pane_renderer: Rc<PaneContentRenderer> = Rc::new(pane_renderer);
        let parent = cx.weak_entity();
        let retry_parent = parent.clone();
        let retry_emitter: Rc<RetryEmitter> = Rc::new(move |tab_id, pane_id, cx| {
            let _ = retry_parent.update(cx, |_, cx| {
                cx.emit(WorkspaceRenderEvent::RetryPane { tab_id, pane_id });
            });
        });
        let reconnect_parent = parent.clone();
        let reconnect_emitter: Rc<ReconnectEmitter> = Rc::new(move |event, cx| {
            let _ = reconnect_parent.update(cx, |_, cx| cx.emit(event));
        });
        let pane_index = Rc::clone(&projection_index);
        let pane_content = Rc::clone(&pane_renderer);
        let pane_retry = Rc::clone(&retry_emitter);
        let pane_reconnect = Rc::clone(&reconnect_emitter);
        let pane_tab_id = tab_id.clone();
        let pane_projection = PaneProjectionContext {
            tab_id: pane_tab_id,
            projections: pane_index,
            renderer: pane_content,
            retry_emitter: pane_retry,
            reconnect_emitter: pane_reconnect,
        };
        let initial_layout = layout.clone();
        let initial_focus = focused_pane.clone();
        let title_index = Rc::clone(&projection_index);
        let header_host_picker = Rc::clone(&host_picker_projection);
        let header_tab_id = tab_id.clone();
        let panes = cx.new(|_| {
            WorkspacePanes::new(
                layout,
                focused_pane,
                move |pane_id, probe_context, window, cx| {
                    render_pane_projection(&pane_projection, pane_id, probe_context, window, cx)
                },
                move |pane_id| match title_index.borrow().lookup(pane_id) {
                    Ok(projection) => PaneHeaderProjection {
                        title: projection.title.clone(),
                        refresh: projection
                            .restart
                            .as_ref()
                            .map(|restart| PaneRefreshProjection {
                                binding: restart.binding.clone(),
                                busy: restart.busy,
                            }),
                        host_picker: header_host_picker.borrow().clone().filter(|picker| {
                            picker.request.tab_id == header_tab_id
                                && picker.request.pane_id == *pane_id
                        }),
                        broadcast: projection.broadcast,
                    },
                    Err(_) => PaneHeaderProjection {
                        title: crate::locale::text("本地终端", "Local terminal"),
                        refresh: None,
                        host_picker: None,
                        broadcast: PaneBroadcastProjection::default(),
                    },
                },
            )
        });
        let event_tab_id = tab_id.clone();
        let pane_subscription =
            cx.subscribe(&panes, move |_, _, event: &WorkspacePaneEvent, cx| {
                cx.emit(WorkspaceRenderEvent::Pane {
                    tab_id: event_tab_id.clone(),
                    event: event.clone(),
                });
            });
        let refresh_tab_id = tab_id.clone();
        let refresh_projection = Rc::clone(&projection_index);
        let pane_refresh_subscription = cx.subscribe(
            &panes,
            move |_, _, event: &WorkspacePaneRefreshEvent, cx| {
                let current_binding = refresh_projection
                    .borrow()
                    .lookup(&event.pane_id)
                    .ok()
                    .and_then(|projection| {
                        projection
                            .restart
                            .filter(|restart| !restart.busy && restart.binding == event.binding)
                            .map(|restart| restart.binding)
                    });
                let Some(binding) = current_binding else {
                    return;
                };
                cx.emit(WorkspaceRenderEvent::RefreshPane {
                    tab_id: refresh_tab_id.clone(),
                    pane_id: event.pane_id.clone(),
                    binding,
                });
            },
        );
        panes.update(cx, |panes, cx| {
            panes.set_projection(
                initial_layout,
                initial_focus,
                candidate.preview,
                candidate.source_bounds,
                candidate.interaction_locked,
                cx,
            );
        });

        Self {
            tab_id,
            projections: projection_index,
            host_picker: host_picker_projection,
            panes,
            _pane_subscription: pane_subscription,
            _pane_refresh_subscription: pane_refresh_subscription,
        }
    }

    /// 候选上下文与布局一起替换；保留 pane Entity，避免快照刷新重置 resizer 或错配测量。
    pub(super) fn set_projection(
        &mut self,
        projection: WorkspaceRenderProjection,
        cx: &mut Context<Self>,
    ) {
        let WorkspaceRenderProjection {
            layout,
            focused_pane,
            panes: projections,
            candidate,
            host_picker,
        } = projection;
        *self.projections.borrow_mut() =
            ProjectionIndex::build(&layout, &focused_pane, projections);
        *self.host_picker.borrow_mut() = host_picker;
        self.panes.update(cx, |panes, cx| {
            panes.set_projection(
                layout,
                focused_pane,
                candidate.preview,
                candidate.source_bounds,
                candidate.interaction_locked,
                cx,
            );
        });
        cx.notify();
    }

    /// Root 在变更 topology 前读取 source 的同代真实 slot Bounds，供 candidate resizer 首帧定尺寸。
    ///
    /// @author kongweiguang
    pub(super) fn measured_pane_bounds(
        &self,
        pane_id: &PaneId,
        cx: &gpui_kit::App,
    ) -> Option<(u64, Bounds<Pixels>)> {
        self.panes.read(cx).measured_pane_bounds(pane_id)
    }

    /// 对 deferred measurement 做精确候选和布局代次比对，避免仅凭新 pane ID 接受迟到帧。
    ///
    /// @author kongweiguang
    pub(super) fn split_candidate_matches(
        &self,
        preview: &SplitCandidatePreview,
        layout_generation: u64,
        cx: &gpui_kit::App,
    ) -> bool {
        self.panes
            .read(cx)
            .candidate_matches(preview, layout_generation)
    }
}

impl Render for WorkspaceRender {
    /// 同步展示投影提示；根 underlay 负责背景图，因此全尺寸工作区只铺半透明 workspace 材料。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        let has_issues = self.projections.borrow().has_issues();
        let mut root = div()
            .id(format!("workspace-render-{}", self.tab_id.as_str()))
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.materials.workspace);

        if has_issues {
            root = root.child(
                div()
                    .id("workspace-projection-warning")
                    .w_full()
                    .px_3()
                    .py_1()
                    .bg(theme.sidebar)
                    .text_color(theme.error)
                    .child(crate::locale::text(
                        "分屏布局与会话状态尚未同步，部分分屏暂不可用。",
                        "Pane layout and session state are out of sync; some panes are unavailable.",
                    )),
            );
        }

        root.child(self.panes.clone())
    }
}

/// 把 layout 和运行态列表按 PaneId 建立一一映射；重复或多余数据不得误画到其他叶子。
struct ProjectionIndex {
    by_pane: BTreeMap<PaneId, WorkspacePaneProjection>,
    expected: BTreeSet<PaneId>,
    missing: BTreeSet<PaneId>,
    duplicates: BTreeSet<PaneId>,
    unexpected_count: usize,
    layout_valid: bool,
    focus_valid: bool,
}

impl ProjectionIndex {
    /// 丢弃重复与非布局成员，防止列表顺序变化把旧 session 快照画进错误 pane。
    fn build(
        layout: &TerminalLayoutNode,
        focused_pane: &PaneId,
        projections: Vec<WorkspacePaneProjection>,
    ) -> Self {
        let layout_valid = layout.validate().is_ok();
        let expected = layout.pane_ids().into_iter().collect::<BTreeSet<_>>();
        let focus_valid = expected.contains(focused_pane);
        let mut by_pane = BTreeMap::new();
        let mut duplicates = BTreeSet::new();
        let mut unexpected_count = 0;

        for projection in projections {
            let pane_id = projection.pane_id.clone();
            if !expected.contains(&pane_id) {
                unexpected_count += 1;
                continue;
            }
            if duplicates.contains(&pane_id) {
                continue;
            }
            if by_pane.insert(pane_id.clone(), projection).is_some() {
                by_pane.remove(&pane_id);
                duplicates.insert(pane_id);
            }
        }

        let missing = expected
            .iter()
            .filter(|pane_id| !by_pane.contains_key(*pane_id) && !duplicates.contains(*pane_id))
            .cloned()
            .collect();

        Self {
            by_pane,
            expected,
            missing,
            duplicates,
            unexpected_count,
            layout_valid,
            focus_valid,
        }
    }

    /// 检查 Root 投影与当前布局是否完整，异常时提示用户而不猜测缺失资源。
    fn has_issues(&self) -> bool {
        !self.layout_valid
            || !self.focus_valid
            || !self.missing.is_empty()
            || !self.duplicates.is_empty()
            || self.unexpected_count != 0
    }

    /// 查找与叶子身份精确关联的投影；未知或歧义项按失败状态呈现。
    ///
    /// @author kongweiguang
    fn lookup(&self, pane_id: &PaneId) -> Result<WorkspacePaneProjection, ProjectionLookupError> {
        if self.duplicates.contains(pane_id) {
            return Err(ProjectionLookupError::Duplicate);
        }
        if self.missing.contains(pane_id) || !self.expected.contains(pane_id) {
            return Err(ProjectionLookupError::Missing);
        }
        self.by_pane
            .get(pane_id)
            .cloned()
            .ok_or(ProjectionLookupError::Missing)
    }
}

/// 查询错误区分重复身份与尚未到达的数据；成功投影由 Result 的 Ok 携带，避免人为的大枚举。
///
/// @author kongweiguang
enum ProjectionLookupError {
    Missing,
    Duplicate,
}

/// 只有真实会话 ID 与同 ID 的终端快照配对时，Root 才能绘制该 pane 的交互画布。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaneSurfaceState {
    Ready,
    Restoring,
    Failed,
    SnapshotSessionMismatch,
    Closing,
}

/// status 在 surface 分类中代表失败；未准入 Pending 只应走 Restoring 文案与测量，不生成伪错误。
/// 已记录的真实错误和失败/关闭兜底仍保留，不能为启动探针而吞掉资源失败。
///
/// @author kongweiguang
pub(super) fn workspace_pane_projection_status(
    phase: Option<RuntimePanePhase>,
    status: Option<SharedString>,
) -> Option<SharedString> {
    status.or_else(|| match phase {
        Some(RuntimePanePhase::RestoreFailed) => Some(crate::locale::text(
            "此分屏恢复失败，可重试。",
            "This pane failed to restore and can be retried.",
        )),
        Some(RuntimePanePhase::CloseFailed) => Some(crate::locale::text(
            "终端未能关闭，可重试关闭。",
            "The terminal did not close and can be retried.",
        )),
        Some(RuntimePanePhase::Closing) => {
            Some(crate::locale::text("正在关闭此分屏…", "Closing this pane…"))
        }
        _ => None,
    })
}

/// 只有仍在执行的关闭才覆盖 pane 文案；CloseFailed 必须保留失败原因与重试提示。
///
/// @author kongweiguang
pub(super) const fn workspace_pane_is_closing(phase: Option<RuntimePanePhase>) -> bool {
    matches!(phase, Some(RuntimePanePhase::Closing))
}

/// 关闭中的临时提示可覆盖状态；失败态必须显示保留的错误与重试说明。
///
/// @author kongweiguang
fn workspace_pane_status_banner(
    status: Option<SharedString>,
    is_closing: bool,
) -> Option<SharedString> {
    if is_closing {
        Some(crate::locale::text("正在关闭此分屏…", "Closing this pane…"))
    } else {
        status
    }
}

/// 根据可见资源状态分类面板，避免显示其他会话的迟到快照或伪造重试完成态。
fn pane_surface_state(projection: &WorkspacePaneProjection) -> PaneSurfaceState {
    classify_surface_state(
        projection.session_id.as_ref(),
        projection
            .snapshot
            .as_ref()
            .map(|snapshot| &snapshot.session_id),
        projection.status.is_some(),
        projection.is_closing,
    )
}

/// 纯状态转换供渲染和单测共用；关闭状态优先于快照，身份不匹配不得显示。
fn classify_surface_state(
    session_id: Option<&SessionId>,
    snapshot_session_id: Option<&SessionId>,
    has_error: bool,
    is_closing: bool,
) -> PaneSurfaceState {
    match (session_id, snapshot_session_id) {
        (Some(session_id), Some(snapshot_session_id)) if session_id != snapshot_session_id => {
            PaneSurfaceState::SnapshotSessionMismatch
        }
        _ if is_closing => PaneSurfaceState::Closing,
        (Some(session_id), Some(snapshot_session_id)) if session_id == snapshot_session_id => {
            PaneSurfaceState::Ready
        }
        _ if has_error => PaneSurfaceState::Failed,
        _ => PaneSurfaceState::Restoring,
    }
}

/// 同一组 `WorkspacePanes` 共享渲染依赖，避免每个 pane 重复传递整组上下文。
///
/// @author kongweiguang
struct PaneProjectionContext {
    tab_id: TabId,
    projections: Rc<RefCell<ProjectionIndex>>,
    renderer: Rc<PaneContentRenderer>,
    retry_emitter: Rc<RetryEmitter>,
    reconnect_emitter: Rc<ReconnectEmitter>,
}

/// 以精确 pane 投影绘制 Root 注入的 canvas；关闭操作拥有的失败必须先停在占位层，避免复用成恢复或候选测量。
///
/// @author kongweiguang
fn render_pane_projection(
    context: &PaneProjectionContext,
    pane_id: &PaneId,
    probe_context: Option<SplitCandidateProbeContext>,
    window: &mut Window,
    cx: &mut Context<WorkspacePanes>,
) -> AnyElement {
    let tab_id = &context.tab_id;
    let projections = &context.projections;
    let renderer = &context.renderer;
    let retry_emitter = &context.retry_emitter;
    let reconnect_emitter = &context.reconnect_emitter;
    let lookup = projections.borrow().lookup(pane_id);
    let projection = match lookup {
        Ok(projection) => projection,
        Err(ProjectionLookupError::Missing) => {
            return render_placeholder(
                pane_id,
                crate::locale::text(
                    "此分屏状态尚未到达，暂不可显示。",
                    "This pane state has not arrived yet and cannot be displayed.",
                ),
                None,
                tab_id,
                retry_emitter,
                window,
                cx,
            );
        }
        Err(ProjectionLookupError::Duplicate) => {
            return render_placeholder(
                pane_id,
                crate::locale::text(
                    "此分屏收到重复会话状态，已暂停绘制。",
                    "Duplicate session state was received for this pane; rendering is paused.",
                ),
                None,
                tab_id,
                retry_emitter,
                window,
                cx,
            );
        }
    };

    if close_operation_blocks_retry_or_measurement(&projection, probe_context.is_some()) {
        return render_placeholder(
            pane_id,
            crate::locale::text(
                "此分屏仍由关闭操作处理，请从关闭/清理入口继续。",
                "This pane is still owned by a close operation. Continue from the close or cleanup controls.",
            ),
            None,
            tab_id,
            retry_emitter,
            window,
            cx,
        );
    }

    if let Some(probe_context) = probe_context {
        if pane_id != &probe_context.preview.pane_id {
            return render_placeholder(
                pane_id,
                crate::locale::text(
                    "分屏测量目标已变化，此占位不会启动终端。",
                    "The split measurement target changed; this placeholder will not start a terminal.",
                ),
                None,
                tab_id,
                retry_emitter,
                window,
                cx,
            );
        }
        if let Some(retry_pane) = candidate_retry_pane(&projection, pane_id) {
            return render_placeholder(
                pane_id,
                projection.status.clone().unwrap_or_else(|| {
                    crate::locale::text(
                        "新分屏启动失败，可安全重试。",
                        "The new pane could not start and can be safely retried.",
                    )
                }),
                Some(retry_pane),
                tab_id,
                retry_emitter,
                window,
                cx,
            );
        }
        if matches!(
            pane_surface_state(&projection),
            PaneSurfaceState::Closing | PaneSurfaceState::SnapshotSessionMismatch
        ) {
            return render_placeholder(
                pane_id,
                projection.status.clone().unwrap_or_else(|| {
                    crate::locale::text(
                        "分屏资源正在收尾，测量已暂停。",
                        "Split resources are being finalized; measurement is paused.",
                    )
                }),
                None,
                tab_id,
                retry_emitter,
                window,
                cx,
            );
        }
        let message = crate::locale::text(
            "正在校验分屏布局；持续未完成时可取消后重试。",
            "Checking the split layout; cancel and retry if it does not finish.",
        );
        let cancel_preview = projection
            .split_candidate_cancelable
            .then(|| (probe_context.preview.clone(), reconnect_emitter));
        let probe = renderer(&projection, Some(probe_context), window, cx);
        return render_placeholder_with_probe(
            PlaceholderProbePresentation {
                pane_id,
                message,
                cancel: cancel_preview,
                tab_id,
                retry_emitter,
            },
            probe,
            window,
            cx,
        );
    }

    match pane_surface_state(&projection) {
        PaneSurfaceState::Ready => {
            render_ready_pane(&projection, tab_id, renderer, reconnect_emitter, window, cx)
        }
        PaneSurfaceState::Closing => {
            if projection.session_id.is_some() && projection.snapshot.is_some() {
                render_ready_pane(&projection, tab_id, renderer, reconnect_emitter, window, cx)
            } else {
                render_placeholder(
                    pane_id,
                    crate::locale::text("正在关闭此分屏…", "Closing this pane…"),
                    None,
                    tab_id,
                    retry_emitter,
                    window,
                    cx,
                )
            }
        }
        PaneSurfaceState::Restoring => {
            let message =
                crate::locale::text("正在恢复此分屏的会话…", "Restoring this pane session…");
            render_placeholder_with_probe(
                PlaceholderProbePresentation {
                    pane_id,
                    message,
                    cancel: None,
                    tab_id,
                    retry_emitter,
                },
                renderer(&projection, None, window, cx),
                window,
                cx,
            )
        }
        PaneSurfaceState::Failed => render_placeholder(
            pane_id,
            projection.status.clone().unwrap_or_else(|| {
                crate::locale::text("此分屏恢复失败。", "This pane could not be restored.")
            }),
            projection
                .target
                .as_ref()
                .map(|_| projection.pane_id.clone()),
            tab_id,
            retry_emitter,
            window,
            cx,
        ),
        PaneSurfaceState::SnapshotSessionMismatch => render_placeholder(
            pane_id,
            crate::locale::text(
                "终端快照与当前会话不匹配，已停止显示。",
                "The terminal snapshot does not match this session and was hidden.",
            ),
            None,
            tab_id,
            retry_emitter,
            window,
            cx,
        ),
    }
}

/// 关闭操作拥有失败 pane 时禁止恢复重试；候选测量也不能在关闭 owner 未收敛时启动。
///
/// @author kongweiguang
fn close_operation_blocks_retry_or_measurement(
    projection: &WorkspacePaneProjection,
    has_candidate_probe: bool,
) -> bool {
    projection.close_operation_pending
        && (has_candidate_probe || pane_surface_state(projection) == PaneSurfaceState::Failed)
}

/// 将关闭操作与 pane 精确关联，避免它的暂态失败覆盖成另一个恢复/分屏操作。
///
/// @author kongweiguang
pub(super) fn close_operation_targets_pane(
    operation: Option<&WorkspaceResourceOperation>,
    pane_id: &PaneId,
) -> bool {
    match operation {
        Some(WorkspaceResourceOperation::ClosePane {
            pane_id: closing_pane,
            ..
        }) => closing_pane == pane_id,
        Some(WorkspaceResourceOperation::CloseTab { pane_ids, .. }) => {
            pane_ids.iter().any(|closing_pane| closing_pane == pane_id)
        }
        Some(WorkspaceResourceOperation::CreatePane { .. }) | None => false,
    }
}

/// 只给失败的真实恢复或分屏创建重试；Close owner 必须沿关闭/清理入口收敛。
///
/// @author kongweiguang
fn candidate_retry_pane(
    projection: &WorkspacePaneProjection,
    candidate_pane_id: &PaneId,
) -> Option<PaneId> {
    (projection.pane_id == *candidate_pane_id
        && !projection.close_operation_pending
        && pane_surface_state(projection) == PaneSurfaceState::Failed)
        .then(|| candidate_pane_id.clone())
}

/// 给有效的单 pane 数据委托给 Root；半透明背景保留根 underlay，重连仍绑定原 session。
///
/// @author kongweiguang
fn render_ready_pane(
    projection: &WorkspacePaneProjection,
    tab_id: &TabId,
    renderer: &Rc<PaneContentRenderer>,
    reconnect_emitter: &Rc<ReconnectEmitter>,
    window: &mut Window,
    cx: &mut Context<WorkspacePanes>,
) -> AnyElement {
    let content = renderer(projection, None, window, cx);
    let banner = workspace_pane_status_banner(projection.status.clone(), projection.is_closing);
    let theme = ThemeTokens::current(window, cx);
    let mut body = div()
        .id(format!(
            "workspace-pane-status-{}",
            projection.pane_id.as_str()
        ))
        .w_full()
        .h_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(theme.materials.workspace);

    if let Some(message) = banner {
        body = body.child(
            div()
                .w_full()
                .px_2()
                .py_1()
                .bg(theme.sidebar)
                .text_color(theme.muted)
                .child(message),
        );
    }

    if let Some(reconnect) = projection.reconnect.as_ref() {
        let binding = reconnect.binding.clone();
        let pane_id = projection.pane_id.clone();
        let tab_id = tab_id.clone();
        let action = Rc::clone(reconnect_emitter);
        let control = match &reconnect.state {
            SessionReconnectState::Available
            | SessionReconnectState::Failed { .. }
            | SessionReconnectState::Cancelled => {
                let event = WorkspaceRenderEvent::ReconnectPane {
                    tab_id,
                    pane_id,
                    binding,
                };
                let action = Rc::clone(&action);
                Button::new(format!(
                    "workspace-pane-reconnect-{}",
                    projection.pane_id.as_str()
                ))
                .compact()
                .label(crate::locale::text("重连", "Reconnect"))
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    action(event.clone(), cx);
                }))
                .into_any_element()
            }
            SessionReconnectState::Connecting { attempt_id } => {
                let event = WorkspaceRenderEvent::CancelReconnect {
                    tab_id,
                    pane_id,
                    binding,
                    attempt_id: *attempt_id,
                };
                let action = Rc::clone(&action);
                Button::new(format!(
                    "workspace-pane-reconnect-cancel-{}",
                    projection.pane_id.as_str()
                ))
                .compact()
                .label(crate::locale::text("取消重连", "Cancel reconnect"))
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    action(event.clone(), cx);
                }))
                .into_any_element()
            }
            SessionReconnectState::Cancelling { .. } => div()
                .px_2()
                .py_1()
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "正在取消重连…",
                    "Cancelling reconnect…",
                ))
                .into_any_element(),
        };

        let mut controls = div()
            .id(format!(
                "workspace-pane-reconnect-bar-{}",
                projection.pane_id.as_str()
            ))
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .bg(theme.sidebar);
        if let SessionReconnectState::Failed { message } = &reconnect.state {
            controls = controls.child(
                div()
                    .flex_1()
                    .text_color(theme.error)
                    .child(SharedString::from(message.clone())),
            );
        }
        body = body.child(controls.child(control));
    }

    body.child(div().flex_1().min_h(px(0.)).child(content))
        .into_any_element()
}

/// 将测量状态卡、候选身份与取消事件桥成组传递，避免拆分参数后发生目标串用。
///
/// @author kongweiguang
struct PlaceholderProbePresentation<'a> {
    pane_id: &'a PaneId,
    message: SharedString,
    cancel: Option<(SplitCandidatePreview, &'a Rc<ReconnectEmitter>)>,
    tab_id: &'a TabId,
    retry_emitter: &'a Rc<RetryEmitter>,
}

/// 按统一展示上下文渲染惰性测量画布及其可选的精确取消动作。
///
/// @author kongweiguang
fn render_placeholder_with_probe(
    presentation: PlaceholderProbePresentation<'_>,
    probe: AnyElement,
    window: &mut Window,
    cx: &mut Context<WorkspacePanes>,
) -> AnyElement {
    let PlaceholderProbePresentation {
        pane_id,
        message,
        cancel,
        tab_id,
        retry_emitter,
    } = presentation;
    let mut body = div()
        .relative()
        .size_full()
        .child(div().absolute().inset_0().child(probe))
        .child(render_placeholder(
            pane_id,
            message,
            None,
            tab_id,
            retry_emitter,
            window,
            cx,
        ));
    if let Some((preview, emitter)) = cancel {
        let emitter = Rc::clone(emitter);
        body = body.child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(px(16.))
                .flex()
                .justify_center()
                .child(
                    Button::new(format!(
                        "workspace-pane-cancel-split-{}",
                        preview.pane_id.as_str()
                    ))
                    .compact()
                    .label(crate::locale::text("取消分屏", "Cancel split"))
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        emitter(
                            WorkspaceRenderEvent::CancelSplitCandidate {
                                preview: preview.clone(),
                            },
                            cx,
                        );
                    })),
                ),
        );
    }
    body.into_any_element()
}

/// 缺失或失败状态占据真实 pane 边界；背景沿用 workspace 材料以免遮住根 underlay。
/// 仅在目标仍可恢复时发布重试意图。
///
/// @author kongweiguang
fn render_placeholder(
    pane_id: &PaneId,
    message: SharedString,
    retry_pane: Option<PaneId>,
    tab_id: &TabId,
    retry_emitter: &Rc<RetryEmitter>,
    window: &mut Window,
    cx: &mut Context<WorkspacePanes>,
) -> AnyElement {
    let theme = ThemeTokens::current(window, cx);
    let mut body = div()
        .id(format!("workspace-pane-placeholder-{}", pane_id.as_str()))
        .w_full()
        .h_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .px_4()
        .bg(theme.materials.workspace)
        .text_color(theme.muted)
        .child(message);

    if let Some(retry_pane) = retry_pane {
        let retry_tab = tab_id.clone();
        let retry_action = Rc::clone(retry_emitter);
        body = body.child(
            Button::new(format!("workspace-pane-retry-{}", retry_pane.as_str()))
                .compact()
                .label(crate::locale::text("重试恢复", "Retry restore"))
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    retry_action(retry_tab.clone(), retry_pane.clone(), cx);
                })),
        );
    }

    body.into_any_element()
}

#[cfg(test)]
#[path = "../../tests/unit/root_workspace_render.rs"]
mod tests;
