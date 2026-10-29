//! 多级终端分屏投影与 GPUI Kit 原生比例拖拽组件。
//!
//! @author kongweiguang

use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap},
    rc::Rc,
};

use gpui_kit::base::ElementExt as _;
use gpui_kit::{
    AnyElement, AppContext as _, Bounds, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    InteractiveElement as _, IntoElement, MouseButton, Pixels, Render, SharedString,
    StatefulInteractiveElement as _, StyleRefinement, Window,
    assets::IconName,
    component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable},
    component::{
        Disableable as _, Sizable as _,
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    identity::{PaneId, TabId},
    sessions::TerminalSize,
    workspace::{SplitDirection, SplitLayoutSizes, TerminalLayoutNode},
};

use crate::{
    features::{
        host_picker::{HostPickerPopoverConfig, HostPickerPopoverProjection, host_picker_popover},
        workspace_broadcast::PaneBroadcastProjection,
    },
    theme::ThemeTokens,
};

/// 分屏叶子由 Root 注入真实终端画布；此组件不创建或销毁任何会话。
pub(crate) type PaneRenderer = dyn for<'a> Fn(
        &PaneId,
        Option<SplitCandidateProbeContext>,
        &mut Window,
        &mut Context<'a, WorkspacePanes>,
    ) -> gpui_kit::AnyElement
    + 'static;

/// Pane 标题与重启目标由 Root 统一投影，不在组件中猜测终端来源。
pub(crate) type PaneTitleRenderer = dyn Fn(&PaneId) -> PaneHeaderProjection + 'static;

/// 标题栏仅展示稳定标题和来自 Root 的精确重启状态，不自行推导会话身份。
///
/// @author kongweiguang
#[derive(Clone)]
pub(crate) struct PaneHeaderProjection {
    pub(crate) title: SharedString,
    pub(crate) refresh: Option<PaneRefreshProjection>,
    pub(crate) host_picker: Option<HostPickerPopoverProjection>,
    pub(crate) broadcast: PaneBroadcastProjection,
}

/// 重启绑定来自当前服务观察；busy 时仍保留相同按钮槽位以固定工具栏布局。
///
/// @author kongweiguang
#[derive(Clone)]
pub(crate) struct PaneRefreshProjection {
    pub(crate) binding: UiSessionBinding,
    pub(crate) busy: bool,
}

/// Root 对未提交 CreatePane 的只读身份投影；candidate 只改变画面，不取得 session owner。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SplitCandidatePreview {
    pub(crate) operation_id: String,
    pub(crate) tab_id: TabId,
    pub(crate) source_pane_id: PaneId,
    pub(crate) pane_id: PaneId,
}

/// Canvas 发回 Root 的精确同代测量回执；Root 仍需重验 controller 与运行态 owner。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SplitCandidateMeasurement {
    pub(crate) preview: SplitCandidatePreview,
    pub(crate) layout_generation: u64,
    pub(crate) size: TerminalSize,
}

/// Candidate Canvas 用从根到目标 split 的同帧几何路径确认可启动网格。
#[derive(Clone)]
pub(crate) struct SplitCandidateProbeContext {
    pub(crate) preview: SplitCandidatePreview,
    pub(crate) layout_generation: u64,
    pub(crate) ancestors: Vec<SplitResizerAncestorProbe>,
}

/// 每层 split 都携带 resizer 状态和全部直系 slot，供 Canvas defer 后统一核验同一帧。
#[derive(Clone)]
pub(crate) struct SplitResizerAncestorProbe {
    pub(crate) split_id: String,
    pub(crate) direction: SplitDirection,
    pub(crate) child_index: usize,
    pub(crate) state: Entity<ResizableState>,
    pub(crate) group_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    pub(crate) child_bounds: Rc<RefCell<Vec<Option<Bounds<Pixels>>>>>,
}

type PaneBoundsCache = Rc<RefCell<HashMap<PaneId, (u64, Bounds<Pixels>)>>>;

/// 仅保留 candidate 分支各层 split 的同帧几何快照；不持有布局或终端资源。
struct CandidateMeasureFrame {
    group_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    child_bounds: Rc<RefCell<Vec<Option<Bounds<Pixels>>>>>,
}

/// 纯用户交互意图由 Root 校验后交给 WorkspaceService 处理。
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspacePaneEvent {
    Activate(PaneId),
    Split {
        pane_id: PaneId,
        direction: SplitDirection,
        before: bool,
    },
    ChooseSplitTarget {
        pane_id: PaneId,
        direction: SplitDirection,
        before: bool,
    },
    CancelSplitTarget {
        pane_id: PaneId,
        direction: SplitDirection,
        before: bool,
        request_generation: u64,
    },
    /// 目标选择由 Root 的当前 Tab 运行态维护，不写入持久分屏布局。
    ToggleBroadcastTarget {
        pane_id: PaneId,
    },
    /// 群发栏关闭后保留同一槽位的重新打开入口，避免窄分屏增加一组工具按钮。
    ShowBroadcast {
        pane_id: PaneId,
    },
    Close(PaneId),
    /// 只由关闭按钮自己的原生键盘 click 生成；瞬时焦点来源由 Root 核对后归一为 Close，不进入资源或持久模型。
    CloseFromKeyboard {
        pane_id: PaneId,
        close_focus: FocusHandle,
    },
    Swap {
        first: PaneId,
        second: PaneId,
    },
    Resize {
        split_id: String,
        sizes: SplitLayoutSizes,
    },
}

/// 顶栏重启动作冻结渲染时的 session 身份，避免迟到点击作用于同一 PaneId 的新会话。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspacePaneRefreshEvent {
    pub(crate) pane_id: PaneId,
    pub(crate) binding: UiSessionBinding,
}

/// 多级布局组件只保留 Root 发布的结构、焦点投影与 terminal canvas renderer。
pub struct WorkspacePanes {
    layout: TerminalLayoutNode,
    layout_generation: u64,
    active_pane: PaneId,
    resizable_states: HashMap<String, Entity<ResizableState>>,
    candidate_preview: Option<SplitCandidatePreview>,
    candidate_split_id: Option<String>,
    candidate_seed_extent: Option<Pixels>,
    candidate_interaction_locked: bool,
    pane_bounds: PaneBoundsCache,
    pane_renderer: Box<PaneRenderer>,
    pane_title_renderer: Box<PaneTitleRenderer>,
}

impl EventEmitter<WorkspacePaneEvent> for WorkspacePanes {}
impl EventEmitter<WorkspacePaneRefreshEvent> for WorkspacePanes {}

impl WorkspacePanes {
    /// 注入已校验布局、真实标题和画布 renderer；持久结构仍由 WorkspaceService 管理。
    ///
    /// @author kongweiguang
    pub(crate) fn new(
        layout: TerminalLayoutNode,
        active_pane: PaneId,
        pane_renderer: impl for<'a> Fn(
            &PaneId,
            Option<SplitCandidateProbeContext>,
            &mut Window,
            &mut Context<'a, WorkspacePanes>,
        ) -> gpui_kit::AnyElement
        + 'static,
        pane_title_renderer: impl Fn(&PaneId) -> PaneHeaderProjection + 'static,
    ) -> Self {
        Self {
            layout,
            layout_generation: 0,
            active_pane,
            resizable_states: HashMap::new(),
            candidate_preview: None,
            candidate_split_id: None,
            candidate_seed_extent: None,
            candidate_interaction_locked: false,
            pane_bounds: Rc::new(RefCell::new(HashMap::new())),
            pane_renderer: Box::new(pane_renderer),
            pane_title_renderer: Box::new(pane_title_renderer),
        }
    }

    /// 切换 committed 或 view-only candidate 投影；锁只遮挡 pending 期输入，不参与 candidate 身份判断。
    ///
    /// source_bounds 必须来自切换前同一 layout generation 的实际 pane prepaint。
    ///
    /// @author kongweiguang
    pub(crate) fn set_projection(
        &mut self,
        layout: TerminalLayoutNode,
        active_pane: PaneId,
        preview: Option<SplitCandidatePreview>,
        source_bounds: Option<(u64, Bounds<Pixels>)>,
        candidate_interaction_locked: bool,
        cx: &mut Context<Self>,
    ) {
        let previous_generation = self.layout_generation;
        let structure_changed = !same_layout_structure(&self.layout, &layout);
        if structure_changed {
            // ResizableState 由 GPUI 按 element ID 缓存；拓扑变更时换 key，防止复用其他 Tab 的拖拽状态。
            self.layout_generation = self.layout_generation.wrapping_add(1);
            self.resizable_states.clear();
            self.pane_bounds.borrow_mut().clear();
        }
        self.layout = layout;
        self.active_pane = active_pane;
        self.candidate_interaction_locked = candidate_interaction_locked;

        if self.candidate_preview != preview {
            self.candidate_preview = preview.clone();
            self.candidate_split_id = preview.as_ref().and_then(|preview| {
                find_candidate_split_id(&self.layout, &preview.source_pane_id, &preview.pane_id)
            });
            self.candidate_seed_extent = preview.as_ref().and_then(|preview| {
                source_bounds
                    .filter(|(generation, _)| *generation == previous_generation)
                    .and_then(|(_, bounds)| {
                        candidate_split_direction(&self.layout, preview)
                            .map(|direction| axis_extent(bounds, direction))
                    })
            });
        } else if self.candidate_seed_extent.is_none() && preview.is_some() && !structure_changed {
            // A same-operation refresh may supply a prepaint measured before the preview began.
            self.candidate_seed_extent = preview.as_ref().and_then(|preview| {
                source_bounds
                    .filter(|(generation, _)| *generation == previous_generation)
                    .and_then(|(_, bounds)| {
                        candidate_split_direction(&self.layout, preview)
                            .map(|direction| axis_extent(bounds, direction))
                    })
            });
        }
        cx.notify();
    }

    /// 读取最后一次真实 pane card prepaint，Root 用于把 candidate seed 绑定到来源视口。
    pub(crate) fn measured_pane_bounds(&self, pane_id: &PaneId) -> Option<(u64, Bounds<Pixels>)> {
        self.pane_bounds.borrow().get(pane_id).copied()
    }

    /// 确认预览身份与拓扑代次仍一致，防止 pane ID 重用或迟到测量启动旧布局。
    pub(crate) fn candidate_matches(
        &self,
        preview: &SplitCandidatePreview,
        layout_generation: u64,
    ) -> bool {
        self.layout_generation == layout_generation
            && self.candidate_preview.as_ref() == Some(preview)
            && self.candidate_split_id.is_some()
    }

    /// 递归绘制 split 树并在候选分支逐层累加同帧探针；只测实际祖先路径，避免猜测 Kit 几何。
    /// 子容器只在相邻分屏之间留半个间距；嵌套布局不叠加外边距，确保与上方工具栏对齐。
    ///
    /// @author kongweiguang
    fn render_node(
        &mut self,
        node: &TerminalLayoutNode,
        pane_count: usize,
        theme: ThemeTokens,
        probe_context: Option<SplitCandidateProbeContext>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        match node {
            TerminalLayoutNode::Pane { pane_id } => self
                .render_pane(pane_id, pane_count, theme, probe_context, window, cx)
                .into_any_element(),
            TerminalLayoutNode::Split {
                id,
                direction,
                children,
                sizes,
            } => {
                let child_ids = children.iter().map(layout_child_id).collect::<Vec<_>>();
                let initial_sizes = normalized_initial_sizes(&child_ids, sizes.as_ref());
                let split_id = id.clone();
                let resizer_key = format!("workspace-split-{}-{id}", self.layout_generation);
                let state = self
                    .resizable_states
                    .entry(resizer_key.clone())
                    .or_insert_with(|| cx.new(|_| ResizableState::default()))
                    .clone();
                let candidate_split = self.candidate_split_id.as_deref() == Some(id.as_str());
                let candidate_child_index = self
                    .candidate_preview
                    .as_ref()
                    .and_then(|preview| candidate_path_child_index(children, &preview.pane_id));
                let candidate_path =
                    candidate_child_index.is_some() && self.candidate_seed_extent.is_some();
                let measure_frame = candidate_path.then(|| CandidateMeasureFrame {
                    group_bounds: Rc::new(RefCell::new(None)),
                    child_bounds: Rc::new(RefCell::new(vec![None; children.len()])),
                });
                let seed_sizes = if candidate_split {
                    self.candidate_seed_extent
                        .map(|extent| initial_panel_sizes(extent, &initial_sizes))
                } else {
                    None
                };
                let resize_ids = child_ids.clone();
                let workspace = cx.weak_entity();
                let on_resize = move |state: &Entity<ResizableState>,
                                      _: &mut Window,
                                      app: &mut gpui_kit::App| {
                    let pixels = state.read(app).sizes().clone();
                    if let Some(sizes) = normalize_child_sizes(&resize_ids, &pixels) {
                        let event = WorkspacePaneEvent::Resize {
                            split_id: split_id.clone(),
                            sizes,
                        };
                        let _ = workspace.update(app, |_, cx| cx.emit(event));
                    }
                };
                let mut group = match direction {
                    SplitDirection::Horizontal => h_resizable(resizer_key),
                    SplitDirection::Vertical => v_resizable(resizer_key),
                }
                .with_state(&state);
                // Candidate resize 只允许临时改 Kit 视图；不能把 view-only preview 写回已提交布局。
                if !candidate_split
                    && self.candidate_preview.is_none()
                    && !self.candidate_interaction_locked
                {
                    group = group.on_resize(on_resize);
                }

                for (index, child) in children.iter().enumerate() {
                    let child_probe = match (
                        self.candidate_preview.as_ref(),
                        candidate_child_index,
                        measure_frame.as_ref(),
                    ) {
                        (Some(preview), Some(candidate_index), Some(frame))
                            if candidate_index == index =>
                        {
                            let mut ancestors = probe_context
                                .as_ref()
                                .filter(|probe| {
                                    &probe.preview == preview
                                        && probe.layout_generation == self.layout_generation
                                })
                                .map(|probe| probe.ancestors.clone())
                                .unwrap_or_default();
                            ancestors.push(SplitResizerAncestorProbe {
                                split_id: id.clone(),
                                direction: *direction,
                                child_index: index,
                                state: state.clone(),
                                group_bounds: frame.group_bounds.clone(),
                                child_bounds: frame.child_bounds.clone(),
                            });
                            Some(SplitCandidateProbeContext {
                                preview: preview.clone(),
                                layout_generation: self.layout_generation,
                                ancestors,
                            })
                        }
                        _ => None,
                    };
                    let child_view =
                        self.render_node(child, pane_count, theme, child_probe, window, cx);
                    let initial_size = seed_sizes
                        .as_ref()
                        .and_then(|sizes| sizes.get(index).copied())
                        .unwrap_or_else(|| {
                            let ratio = initial_sizes.get(index).copied().unwrap_or(1.0);
                            // Candidate topology must never inherit the Kit's 1000px placeholder basis.
                            px(if candidate_split {
                                100.0
                            } else {
                                (ratio * 1_000.0).max(120.0) as f32
                            })
                        });
                    let before = if index == 0 { 0. } else { 4. };
                    let after = if index + 1 == children.len() { 0. } else { 4. };
                    let mut panel_slot = div().size_full().flex().flex_col();
                    panel_slot = match direction {
                        SplitDirection::Horizontal => panel_slot.pl(px(before)).pr(px(after)),
                        SplitDirection::Vertical => panel_slot.pt(px(before)).pb(px(after)),
                    };
                    if let Some(frame) = measure_frame.as_ref() {
                        let bounds = frame.child_bounds.clone();
                        panel_slot = panel_slot.on_prepaint(move |slot, _, _| {
                            if let Some(measured) = bounds.borrow_mut().get_mut(index) {
                                *measured = Some(slot);
                            }
                        });
                    }
                    group = group.child(
                        resizable_panel()
                            .size(initial_size)
                            .child(panel_slot.child(child_view)),
                    );
                }
                let element = group.into_any_element();
                if let Some(frame) = measure_frame {
                    let bounds = frame.group_bounds;
                    div()
                        .size_full()
                        .on_prepaint(move |group, _, _| {
                            *bounds.borrow_mut() = Some(group);
                        })
                        .child(element)
                        .into_any_element()
                } else {
                    element
                }
            }
        }
    }

    /// 每个叶子提供圆角终端卡片；标题只占剩余宽度并截断，避免窄分屏把控制区和正文挤变形。
    /// 关闭与目标勾选不冒泡激活 pane；群发勾选与重新打开入口共用固定槽位，保持窄分屏控制区可达。
    /// 键盘关闭只从按钮原生回调带回真实焦点来源，供 Root 在 ACK 后做受限交接。
    ///
    /// @author kongweiguang
    fn render_pane(
        &self,
        pane_id: &PaneId,
        pane_count: usize,
        theme: ThemeTokens,
        probe_context: Option<SplitCandidateProbeContext>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let candidate_pane = self
            .candidate_preview
            .as_ref()
            .is_some_and(|preview| preview.pane_id == *pane_id);
        let preview_locked = self.candidate_preview.is_some();
        let pane_for_click = pane_id.clone();
        let pane_for_drag = pane_id.clone();
        let pane_for_swap = pane_id.clone();
        let pane_for_close = pane_id.clone();
        let header_projection = (self.pane_title_renderer)(pane_id);
        let title = header_projection.title;
        let tooltip_title = title.clone();
        let active = *pane_id == self.active_pane;
        let background = if active {
            theme.materials.panel
        } else {
            theme.materials.workspace
        };
        let border = if active { theme.selected } else { theme.border };

        let mut header =
            div()
                .id(format!("workspace-pane-header-{}", pane_id.as_str()))
                .h(px(32.))
                .w_full()
                .min_w(px(0.))
                .px_2()
                .flex()
                .items_center()
                .gap(px(2.))
                .bg(theme.materials.terminal_header)
                .text_color(if active {
                    theme.foreground
                } else {
                    theme.muted
                })
                .text_size(px(12.))
                .children((pane_count >= 2 && !preview_locked).then(|| {
                    pane_broadcast_control(pane_id.clone(), header_projection.broadcast, cx)
                }))
                .child(
                    div()
                        .id(format!("workspace-pane-title-{}", pane_id.as_str()))
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .tooltip(move |window, cx| {
                            Tooltip::new(tooltip_title.clone()).build(window, cx)
                        })
                        .child(title.clone()),
                );

        if candidate_pane {
            header = header.child(crate::locale::text(
                "正在准备新分屏…",
                "Preparing new pane…",
            ));
        } else if !preview_locked {
            header = header
                .child(pane_refresh_button(
                    pane_id.clone(),
                    header_projection.refresh,
                    cx,
                ))
                .child(split_button(
                    SplitButtonSpec {
                        id: format!("pane-split-horizontal-after-{}", pane_id.as_str()),
                        icon: split_button_icon(SplitDirection::Horizontal),
                        label: crate::locale::text("在右侧分屏", "Split right"),
                        tooltip: crate::locale::text(
                            "向右分屏；右键选择要加入的主机",
                            "Split right with a host selected by right-click",
                        ),
                        pane_id: pane_id.clone(),
                        primary: SplitDirectionIntent {
                            direction: SplitDirection::Horizontal,
                            before: false,
                        },
                    },
                    header_projection.host_picker.clone(),
                    cx,
                ))
                .child(split_button(
                    SplitButtonSpec {
                        id: format!("pane-split-vertical-after-{}", pane_id.as_str()),
                        icon: split_button_icon(SplitDirection::Vertical),
                        label: crate::locale::text("在下方分屏", "Split below"),
                        tooltip: crate::locale::text(
                            "向下分屏；右键选择要加入的主机",
                            "Split below with a host selected by right-click",
                        ),
                        pane_id: pane_id.clone(),
                        primary: SplitDirectionIntent {
                            direction: SplitDirection::Vertical,
                            before: false,
                        },
                    },
                    header_projection.host_picker,
                    cx,
                ));

            let close_label = if pane_count > 1 {
                crate::locale::text("关闭分屏", "Close pane")
            } else {
                crate::locale::text("关闭终端标签", "Close terminal tab")
            };
            header = header.child(
                Button::new(format!("pane-close-{}", pane_id.as_str()))
                    .compact()
                    .ghost()
                    .icon(IconName::Close)
                    .w(px(22.))
                    .h(px(22.))
                    .accessibility_label(close_label.clone())
                    .tooltip(close_label)
                    .tab_stop(true)
                    .on_click(cx.listener(move |_, event: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        if event.is_keyboard() {
                            // GPUI 已核对按钮的按下/释放焦点代次；只在这个关闭回调读取来源，不接受 Root 对任意按钮的猜测。
                            if let Some(close_focus) = window.focused(cx) {
                                cx.emit(WorkspacePaneEvent::CloseFromKeyboard {
                                    pane_id: pane_for_close.clone(),
                                    close_focus,
                                });
                            }
                        } else {
                            cx.emit(WorkspacePaneEvent::Close(pane_for_close.clone()));
                        }
                    })),
            );
        }

        let renderer = (self.pane_renderer)(pane_id, probe_context, window, cx);
        let preview_title = title;

        let pane_bounds = self.pane_bounds.clone();
        let measured_pane_id = pane_id.clone();
        let pane_generation = self.layout_generation;
        let mut pane = div()
            .id(format!("workspace-pane-{}", pane_id.as_str()))
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(theme.radius_panel)
            .border_1()
            .border_color(border)
            .bg(background)
            .child(header)
            .child(
                div()
                    .id(format!("workspace-pane-content-{}", pane_id.as_str()))
                    .flex_1()
                    .min_h(px(0.))
                    .child(renderer),
            )
            .on_prepaint(move |bounds, _, _| {
                pane_bounds
                    .borrow_mut()
                    .insert(measured_pane_id.clone(), (pane_generation, bounds));
            });

        if !preview_locked {
            pane = pane
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    cx.emit(WorkspacePaneEvent::Activate(pane_for_click.clone()));
                }))
                .on_drag(pane_for_drag, move |_, _, _, cx| {
                    cx.new(|_| PaneDragPreview {
                        title: preview_title.clone(),
                    })
                })
                .drag_over::<PaneId>(move |style, _, _, _| {
                    style.border_2().border_color(theme.accent)
                })
                .on_drop(cx.listener(move |_, dragged: &PaneId, _, cx| {
                    if dragged != &pane_for_swap {
                        cx.emit(WorkspacePaneEvent::Swap {
                            first: dragged.clone(),
                            second: pane_for_swap.clone(),
                        });
                    }
                }));
        }
        pane
    }
}

/// 勾选和重新打开共用 22px 命中区；回调只提交意图，阻止冒泡以保留“当前目标”的原焦点语义。
///
/// @author kongweiguang
fn pane_broadcast_control(
    pane_id: PaneId,
    projection: PaneBroadcastProjection,
    cx: &mut Context<WorkspacePanes>,
) -> AnyElement {
    if projection.visible {
        Checkbox::new(format!("pane-broadcast-target-{}", pane_id.as_str()))
            .w(px(22.))
            .h(px(22.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .checked(projection.selected)
            .disabled(!projection.enabled)
            .accessibility_label(crate::locale::text(
                "选择群发目标",
                "Select broadcast target",
            ))
            .tooltip(crate::locale::text(
                "选择群发目标",
                "Select broadcast target",
            ))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_change(cx.listener(move |_, _, _, cx| {
                cx.stop_propagation();
                cx.emit(WorkspacePaneEvent::ToggleBroadcastTarget {
                    pane_id: pane_id.clone(),
                });
            }))
            .into_any_element()
    } else {
        Button::new(format!("pane-show-broadcast-{}", pane_id.as_str()))
            .compact()
            .ghost()
            .icon(IconName::Send)
            .w(px(22.))
            .h(px(22.))
            .flex_none()
            .accessibility_label(crate::locale::text("显示群发栏", "Show broadcast bar"))
            .tooltip(crate::locale::text("显示群发栏", "Show broadcast bar"))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.stop_propagation();
                cx.emit(WorkspacePaneEvent::ShowBroadcast {
                    pane_id: pane_id.clone(),
                });
            }))
            .into_any_element()
    }
}

impl Render for WorkspacePanes {
    /// 无效恢复布局展示可见错误；候选局部透明盾拦截分隔线，配色与工作台共享 main 投影。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx).for_workbench();
        if self.layout.validate().is_err() {
            return div()
                .id("workspace-invalid-layout")
                .w_full()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.materials.workspace)
                .text_color(theme.error)
                .child(crate::locale::text(
                    "终端分屏布局无效，请从工作区恢复会话",
                    "The terminal pane layout is invalid. Restore the session from the workspace.",
                ))
                .into_any_element();
        }
        let pane_count = self.layout.pane_ids().len();
        let content = self.render_node(&self.layout.clone(), pane_count, theme, None, window, cx);
        if self.candidate_interaction_locked {
            div()
                .size_full()
                .relative()
                .child(content)
                .child(
                    div()
                        .id("workspace-candidate-interaction-shield")
                        .absolute()
                        .inset_0()
                        .bg(theme.materials.workspace.opacity(0.))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(|_, _, cx| cx.stop_propagation()),
                )
                .into_any_element()
        } else {
            content
        }
    }
}

/// 以相邻 child 的稳定 ID 归一化 UI 实际尺寸，忽略 0、无穷与尚未布局完成的快照。
pub fn normalize_child_sizes(child_ids: &[String], pixels: &[Pixels]) -> Option<SplitLayoutSizes> {
    if child_ids.len() < 2 || child_ids.len() != pixels.len() {
        return None;
    }
    let unique_ids = child_ids.iter().collect::<BTreeSet<_>>();
    if unique_ids.len() != child_ids.len() || child_ids.iter().any(|id| id.is_empty()) {
        return None;
    }
    let raw_sizes = pixels.iter().map(|size| size.to_f64()).collect::<Vec<_>>();
    if raw_sizes
        .iter()
        .any(|size| !size.is_finite() || *size <= 0.0)
    {
        return None;
    }
    let total = raw_sizes.iter().sum::<f64>();
    if !total.is_finite() || total <= 0.0 {
        return None;
    }
    Some(
        child_ids
            .iter()
            .cloned()
            .zip(raw_sizes.into_iter().map(|size| size / total))
            .collect(),
    )
}

/// 从已校验的权重映射建立稳定初始比例，错误或缺失权重回退为均分显示。
fn normalized_initial_sizes(child_ids: &[String], stored: Option<&SplitLayoutSizes>) -> Vec<f64> {
    let values = child_ids
        .iter()
        .map(|id| {
            stored
                .and_then(|sizes| sizes.get(id))
                .copied()
                .unwrap_or(1.0)
        })
        .collect::<Vec<_>>();
    let total = values.iter().sum::<f64>();
    if !total.is_finite()
        || total <= 0.0
        || values
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0)
    {
        let equal = 1.0 / child_ids.len().max(1) as f64;
        return vec![equal; child_ids.len()];
    }
    values.into_iter().map(|value| value / total).collect()
}

/// 使用既有 workspace v3 键规则关联比例和布局 child，不依赖绘制顺序生成身份。
fn layout_child_id(node: &TerminalLayoutNode) -> String {
    match node {
        TerminalLayoutNode::Pane { pane_id } => pane_id.as_str().to_owned(),
        TerminalLayoutNode::Split { id, .. } => id.clone(),
    }
}

/// 找到包含 candidate pane 的直系 child，保持 Resizable slot 顺序与递归探针路径一致。
///
/// @author kongweiguang
fn candidate_path_child_index(
    children: &[TerminalLayoutNode],
    candidate_pane_id: &PaneId,
) -> Option<usize> {
    children
        .iter()
        .position(|child| child.contains_pane(candidate_pane_id))
}

/// 比较 split 拓扑但忽略 resize 比例，只有 pane/tree 结构变化才重建控件库的缓存状态。
fn same_layout_structure(first: &TerminalLayoutNode, second: &TerminalLayoutNode) -> bool {
    match (first, second) {
        (
            TerminalLayoutNode::Pane { pane_id: first_id },
            TerminalLayoutNode::Pane { pane_id: second_id },
        ) => first_id == second_id,
        (
            TerminalLayoutNode::Split {
                id: first_id,
                direction: first_direction,
                children: first_children,
                ..
            },
            TerminalLayoutNode::Split {
                id: second_id,
                direction: second_direction,
                children: second_children,
                ..
            },
        ) => {
            first_id == second_id
                && first_direction == second_direction
                && first_children.len() == second_children.len()
                && first_children
                    .iter()
                    .zip(second_children)
                    .all(|(first, second)| same_layout_structure(first, second))
        }
        _ => false,
    }
}

/// 找到直接将候选新 pane 与其来源分开的最深 resizable 节点，避免在祖先组上误测。
///
/// @author kongweiguang
fn find_candidate_split_id(
    layout: &TerminalLayoutNode,
    source_pane_id: &PaneId,
    candidate_pane_id: &PaneId,
) -> Option<String> {
    let TerminalLayoutNode::Split { id, children, .. } = layout else {
        return None;
    };
    for child in children {
        if let Some(found) = find_candidate_split_id(child, source_pane_id, candidate_pane_id) {
            return Some(found);
        }
    }
    let source_child = children
        .iter()
        .position(|child| child.contains_pane(source_pane_id));
    let candidate_child = children
        .iter()
        .position(|child| child.contains_pane(candidate_pane_id));
    (source_child.is_some() && candidate_child.is_some() && source_child != candidate_child)
        .then(|| id.clone())
}

/// 确认候选 split 方向，供来源 slot 的真实逻辑尺寸选择相同轴向。
///
/// @author kongweiguang
fn candidate_split_direction(
    layout: &TerminalLayoutNode,
    preview: &SplitCandidatePreview,
) -> Option<SplitDirection> {
    let TerminalLayoutNode::Split { id, direction, .. } = layout else {
        return None;
    };
    if find_candidate_split_id(layout, &preview.source_pane_id, &preview.pane_id).as_deref()
        == Some(id.as_str())
    {
        return Some(*direction);
    }
    match layout {
        TerminalLayoutNode::Split { children, .. } => children
            .iter()
            .find_map(|child| candidate_split_direction(child, preview)),
        TerminalLayoutNode::Pane { .. } => None,
    }
}

/// 将来源 pane 的同代真实逻辑 Bounds 投影到候选 split 轴，供 Kit 首帧按像素初始化。
///
/// @author kongweiguang
fn axis_extent(bounds: Bounds<Pixels>, direction: SplitDirection) -> Pixels {
    match direction {
        SplitDirection::Horizontal => bounds.size.width,
        SplitDirection::Vertical => bounds.size.height,
    }
}

/// 用真实来源 slot 和候选比例避免 Kit 的默认大面板 basis；100px 下限沿用 Kit 面板约束。
///
/// @author kongweiguang
fn initial_panel_sizes(extent: Pixels, ratios: &[f64]) -> Vec<Pixels> {
    let total = extent.to_f64();
    if !total.is_finite() || total <= 0.0 || ratios.is_empty() {
        return Vec::new();
    }
    ratios
        .iter()
        .map(|ratio| px((total * ratio).max(100.0) as f32))
        .collect()
}

/// 分屏方向只需携带领域方向和前后位置，供主按钮与右键菜单共用。
///
/// @author kongweiguang
#[derive(Clone, Copy)]
struct SplitDirectionIntent {
    direction: SplitDirection,
    before: bool,
}

/// 按钮、焦点目标与反向菜单意图应来自同一 pane 快照。
///
/// @author kongweiguang
struct SplitButtonSpec {
    id: String,
    icon: IconName,
    label: SharedString,
    tooltip: SharedString,
    pane_id: PaneId,
    primary: SplitDirectionIntent,
}

/// 两个固定按钮各自确定分屏方向；右键只锚定主机列表，并冻结原 pane 与请求代次。
///
/// @author kongweiguang
fn split_button(
    spec: SplitButtonSpec,
    picker: Option<HostPickerPopoverProjection>,
    cx: &mut Context<WorkspacePanes>,
) -> impl IntoElement {
    let SplitButtonSpec {
        id,
        icon,
        label,
        tooltip,
        pane_id,
        primary,
    } = spec;
    let view = cx.entity();
    let picker_matches = picker.as_ref().is_some_and(|picker| {
        picker.request.pane_id == pane_id
            && picker.request.direction == primary.direction
            && picker.request.before == primary.before
    });
    let open = picker_matches;
    let request_generation = picker
        .as_ref()
        .map_or(0, |picker| picker.request.generation);
    let focus_handle = picker.as_ref().map(|picker| picker.focus_handle.clone());
    let content = picker.as_ref().map(|picker| picker.content.clone());
    let choose_pane_id = pane_id.clone();
    host_picker_popover(HostPickerPopoverConfig {
        id: id.clone().into(),
        trigger: Button::new(format!("{id}-trigger"))
            .compact()
            .ghost()
            .icon(icon)
            .w(px(22.))
            .h(px(22.))
            .accessibility_label(label.clone())
            .tooltip(tooltip)
            .tab_stop(true)
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                cx.emit(split_event(pane_id.clone(), primary));
            })),
        mouse_button: MouseButton::Right,
        open,
        trigger_style: StyleRefinement::default().w(px(22.)).h(px(22.)).flex_none(),
        focus_handle,
        on_open_change: move |open: &bool, _: &mut Window, app: &mut gpui_kit::App| {
            let pane_id = choose_pane_id.clone();
            let generation = request_generation;
            view.update(app, |_, cx| {
                if *open {
                    cx.emit(split_target_request_event(pane_id, primary));
                } else if picker_matches {
                    cx.emit(WorkspacePaneEvent::CancelSplitTarget {
                        pane_id,
                        direction: primary.direction,
                        before: primary.before,
                        request_generation: generation,
                    });
                }
            });
        },
        content: move |_: &mut gpui_kit::component::popover::PopoverState,
                       _: &mut Window,
                       _: &mut Context<gpui_kit::component::popover::PopoverState>|
              -> AnyElement {
            content
                .clone()
                .map(|content| content.into_any_element())
                .unwrap_or_else(|| div().id("split-target-picker-pending").into_any_element())
        },
    })
}

/// 重启会影响当前 shell，保留明确文字以区别数据刷新；忙碌或 binding 缺失时不发出过期操作。
///
/// @author kongweiguang
fn pane_refresh_button(
    pane_id: PaneId,
    refresh: Option<PaneRefreshProjection>,
    cx: &mut Context<WorkspacePanes>,
) -> impl IntoElement {
    let enabled = refresh.as_ref().is_some_and(|refresh| !refresh.busy);
    let binding = refresh.map(|refresh| refresh.binding);
    let label = crate::locale::text("重启当前终端", "Restart terminal");
    let mut button = Button::new(format!("pane-refresh-{}", pane_id.as_str()))
        .compact()
        .ghost()
        .small()
        .label(crate::locale::text("重启", "Restart"))
        .flex_shrink_0()
        .accessibility_label(label.clone())
        .tooltip(label)
        .tab_stop(true)
        .disabled(!enabled);
    if enabled && let Some(binding) = binding {
        button = button.on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
            cx.stop_propagation();
            cx.emit(WorkspacePaneRefreshEvent {
                pane_id: pane_id.clone(),
                binding: binding.clone(),
            });
        }));
    }
    button
}

/// 分屏方向映射到不会暗示抽屉开合的固定图标，避免重绘后图形视觉反转。
///
/// @author kongweiguang
fn split_button_icon(direction: SplitDirection) -> IconName {
    match direction {
        SplitDirection::Horizontal => IconName::Columns2,
        SplitDirection::Vertical => IconName::Rows2,
    }
}

/// 将主按钮或右键方向转换为相同类型化事件，始终保留菜单打开时的 PaneId。
///
/// @author kongweiguang
fn split_event(pane_id: PaneId, intent: SplitDirectionIntent) -> WorkspacePaneEvent {
    WorkspacePaneEvent::Split {
        pane_id,
        direction: intent.direction,
        before: intent.before,
    }
}

/// 显式目标选择保留触发按钮的方向与来源，不在菜单打开或选择行时激活终端 pane。
///
/// @author kongweiguang
fn split_target_request_event(pane_id: PaneId, intent: SplitDirectionIntent) -> WorkspacePaneEvent {
    WorkspacePaneEvent::ChooseSplitTarget {
        pane_id,
        direction: intent.direction,
        before: intent.before,
    }
}

/// 分屏拖动预览只展示 Pane 标识，不克隆终端 Entity 或运行态资源。
struct PaneDragPreview {
    title: SharedString,
}

impl Render for PaneDragPreview {
    /// 拖动卡片同步主窗口主题，拖拽不会闪烁到组件库默认配色。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx).for_workbench();
        div()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(theme.materials.panel)
            .text_color(theme.foreground)
            .child(self.title.clone())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/workspace_panes_directions.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/unit/workspace_panes_candidate_path.rs"]
mod candidate_path_tests;
