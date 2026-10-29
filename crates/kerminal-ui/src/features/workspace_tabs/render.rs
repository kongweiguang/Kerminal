//! 工作区标签、分组和拖放区域的 GPUI 绘制实现。
//!
//! @author kongweiguang

use std::{cell::RefCell, rc::Rc, time::Duration};

use gpui_kit::base::ElementExt as _;
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, Render, SharedString,
    StatefulInteractiveElement as _, StyleRefinement, Window, WindowControlArea,
    component::button::{Button, ButtonVariants as _},
    component::tooltip::Tooltip,
    component::{
        FocusableExt as _, IconName,
        menu::{ContextMenuExt as _, DropdownMenu as _},
    },
    div,
    prelude::*,
    px,
};

use self::menus::{event_item, group_action_menu, tab_action_menu, tab_kind_label};
use super::{
    TabId, WorkspaceTabEvent, WorkspaceTabGroupProjection, WorkspaceTabKind,
    WorkspaceTabTrackSection, WorkspaceTabs, WorkspaceTabsCreateEvent,
    should_enable_ungrouped_drop_target, should_show_workspace_group_header, tab_drop_event,
    tab_group_color_tint, trailing_tab_drop_event, ungrouped_tab_drop_event,
    workspace_tab_track_width_budget,
};
use crate::features::host_picker::{
    HostPickerGroup, HostPickerList, HostPickerListMessage, HostPickerOption,
    HostPickerPopoverConfig, host_picker_popover,
};
use crate::theme::ThemeTokens;

pub(super) mod menus;

/// 一帧内共享真实标签边界；只保存视图测量，不持有会话或覆盖用户滚动。
struct TabRevealMeasurement {
    tab_id: TabId,
    bounds: Rc<RefCell<Option<gpui_kit::Bounds<gpui_kit::Pixels>>>>,
}

/// 把同一组的绘制决策放在一起，避免多个布尔参数和异步测量目标发生错配。
struct WorkspaceTabGroupRenderOptions {
    show_header: bool,
    is_ungrouped_drop_target: bool,
    tab_reveal: Option<TabRevealMeasurement>,
}

/// 键盘目标顺序只包含已挂载的标签，同时保留可供程序化聚焦的稳定句柄。
type WorkspaceTabKeyboardTargets = Rc<Vec<(TabId, FocusHandle)>>;

const WORKSPACE_TAB_BUTTON_WIDTH: f32 = 28.;
const WORKSPACE_TAB_GAP: f32 = 4.;
const WORKSPACE_TAB_HEIGHT: f32 = 36.;
const WORKSPACE_TAB_PADDING_X: f32 = 10.;
const WORKSPACE_TAB_CONTENT_GAP: f32 = 8.;
const WORKSPACE_TAB_DRAG_SCROLL_INTERVAL: Duration = Duration::from_millis(50);

/// 固定一帧内的新建目标目录、搜索和 owner，避免列表内容与重试来源来自不同快照。
///
/// @author kongweiguang
struct WorkspaceCreateTargetListProjection {
    tabs: Entity<WorkspaceTabs>,
    navigation: Option<Entity<crate::features::connections::HostNavigation>>,
    search_input: gpui_kit::Entity<gpui_kit::component::input::InputState>,
    query: String,
    options: Vec<crate::features::workspace_controller::WorkspaceSplitTargetOption>,
    groups: Vec<kerminal_application::connections::management::HostGroup>,
    loading: bool,
    directory_status: Option<String>,
    theme: ThemeTokens,
}

/// 将标签栏新建候选投影到共享列表；目标选择只发布 typed ref，资源创建仍由 Root 校验并执行。
///
/// @author kongweiguang
fn workspace_create_target_list(
    projection: WorkspaceCreateTargetListProjection,
) -> HostPickerList<kerminal_application::workspace::WorkspaceTargetRef> {
    let WorkspaceCreateTargetListProjection {
        tabs,
        navigation,
        search_input,
        query,
        options,
        groups,
        loading,
        directory_status,
        theme,
    } = projection;
    let list_options = options
        .iter()
        .map(|option| HostPickerOption {
            key: format!("{:?}", option.target).into(),
            target: option.target.clone(),
            title: option.title.clone().into(),
            detail: option.detail.clone().map(Into::into),
            group_id: option.group_id.clone(),
        })
        .collect::<Vec<_>>();
    let list_groups = groups
        .into_iter()
        .map(|group| HostPickerGroup {
            id: group.id,
            title: group.name.into(),
        })
        .collect::<Vec<_>>();
    let owner = tabs.clone();
    let mut list = HostPickerList::new(
        "workspace-tabs-create-target-list",
        list_options,
        move |target, window, app| {
            owner.update(app, |tabs, cx| {
                tabs.set_create_target_picker_open(false, window, cx);
                cx.emit(WorkspaceTabsCreateEvent::Target(target));
            });
            window.prevent_default();
        },
    )
    .search_input(search_input)
    .query(query)
    .groups(list_groups)
    .theme(theme);
    if loading {
        list = list.message(HostPickerListMessage::Loading(crate::locale::text(
            "正在加载主机列表…",
            "Loading hosts…",
        )));
    } else if let Some(status) = directory_status {
        list = list.message(HostPickerListMessage::Error(status.into()));
        if let Some(navigation) = navigation {
            list = list.footer(
                Button::new("workspace-tabs-create-target-retry")
                    .compact()
                    .ghost()
                    .label(crate::locale::text("重试加载", "Retry loading"))
                    .on_click(move |_, window, app| {
                        navigation.update(app, |navigation, cx| navigation.reload(cx));
                        window.prevent_default();
                    }),
            );
        }
    } else if options.is_empty() {
        list = list.message(HostPickerListMessage::Empty(crate::locale::text(
            "没有可用于新建终端的主机。",
            "No hosts are available for a new terminal.",
        )));
    }
    list
}

/// 溢出状态由真实 scroll viewport 测量决定；退出时回补 picker 槽位，避免按钮自身制造溢出。
///
/// @author kongweiguang
pub(super) fn workspace_tab_picker_should_show(
    max_offset: gpui_kit::Pixels,
    picker_visible: bool,
) -> bool {
    let release_threshold = if picker_visible {
        // 隐藏 picker 后，右侧终端按钮回到滚动轨道；回补 picker 宽度和相邻间距消除自致溢出。
        px(WORKSPACE_TAB_BUTTON_WIDTH + WORKSPACE_TAB_GAP)
    } else {
        px(0.)
    };
    max_offset > release_threshold
}

/// 标签拖拽预览只显示标题，不携带终端或文件资源，避免拖动期间误持有会话。
struct TabDragPreview {
    title: SharedString,
}

impl Render for TabDragPreview {
    /// 用简短预览反馈拖动目标，不重建原标签或其关联运行态。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx).for_workbench();
        div()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(theme.panel)
            .text_color(theme.foreground)
            .child(self.title.clone())
    }
}

/// 分组拖动使用独立 payload，避免将组身份误解为标签身份。
#[derive(Clone, Debug, Eq, PartialEq)]
struct GroupDragPayload(String);

/// 分组拖拽预览只呈现用户可见名称，组 ID 仍只用于稳定绑定。
struct GroupDragPreview {
    title: SharedString,
}

impl Render for GroupDragPreview {
    /// 为分组排序显示轻量预览，拖动不会变更持久化快照。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx).for_workbench();
        div()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(theme.panel)
            .text_color(theme.foreground)
            .child(self.title.clone())
    }
}

/// 共用可见组名投影，使绘制与滚动宽度预算对本地化/不可用组回退保持一致。
///
/// @author kongweiguang
fn workspace_group_title(
    group_id: Option<&str>,
    group: Option<&WorkspaceTabGroupProjection>,
) -> SharedString {
    group.map(|group| group.title.clone()).unwrap_or_else(|| {
        if group_id.is_some() {
            crate::locale::text("不可用分组", "Unavailable group")
        } else {
            crate::locale::text("未分组", "Ungrouped")
        }
    })
}

/// 当前标签与相邻项均未激活或悬停时才显示竖分隔线，避免线条穿过高亮表面。
///
/// @author kongweiguang
pub(super) fn should_draw_tab_separator(
    left: &super::WorkspaceTabProjection,
    right: &super::WorkspaceTabProjection,
    hovered_tab_id: Option<&TabId>,
) -> bool {
    !left.active
        && !right.active
        && hovered_tab_id.is_none_or(|hovered| hovered != &left.id && hovered != &right.id)
}

/// 复用导航栏统一的细线几何，仅切换分隔色，避免选中/悬停时相邻标签发生位移。
///
/// @author kongweiguang
fn tab_separator(
    left: &super::WorkspaceTabProjection,
    right: &super::WorkspaceTabProjection,
    hovered_tab_id: Option<&TabId>,
    theme: ThemeTokens,
) -> gpui_kit::AnyElement {
    let color = if should_draw_tab_separator(left, right, hovered_tab_id) {
        theme.border
    } else {
        theme.materials.chrome.opacity(0.)
    };
    crate::features::navigation_chrome::navigation_separator(color)
}

/// 滚动内容必须能收缩到宿主剩余宽度并在两轴裁切，不能盖住左侧折叠入口或右端列表。
/// 列表按钮放在此 viewport 之外；滚轮与自动定位继续共用原 ScrollHandle，不新增导航状态。
/// 滚动 API 要求有身份的 Stateful 元素；稳定 ID 只在此设置一次，调用方不再重复包装。
///
/// @author kongweiguang
pub(super) fn workspace_tabs_scroll_viewport() -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id("workspace-tabs-scroll-area")
        .flex_1()
        .min_w(px(0.))
        .h_full()
        .overflow_x_scroll()
        .overflow_y_hidden()
}

impl WorkspaceTabs {
    /// 焦点句柄按稳定 TabId 保留；新标签才分配句柄，关闭后删除以避免悬空焦点。
    ///
    /// @author kongweiguang
    fn reconcile_tab_focus_handles(&mut self, cx: &mut Context<Self>) {
        self.tab_focus_handles
            .retain(|tab_id, _| self.projection.iter().any(|tab| &tab.id == tab_id));
        for tab in &self.projection {
            self.tab_focus_handles
                .entry(tab.id.clone())
                .or_insert_with(|| cx.focus_handle());
        }
    }

    /// 每次有效拖动最多运行一个短间隔轮询；静止停在边缘时仍可滚动，且不把动画任务带出窗口生命周期。
    /// 窗口失焦、GPUI 结束拖动或源标签/分组失效都会使循环退出；实际投放回调立即撤销任务句柄。
    ///
    /// @author kongweiguang
    fn start_drag_edge_scroll(
        &mut self,
        source: super::WorkspaceTabDragSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.drag_scroll_source.as_ref() == Some(&source) && self.drag_scroll_task.is_some() {
            return;
        }
        self.stop_drag_edge_scroll();
        self.drag_scroll_generation = self.drag_scroll_generation.wrapping_add(1).max(1);
        let generation = self.drag_scroll_generation;
        self.drag_scroll_source = Some(source.clone());
        let scroll_handle = self.scroll_handle.clone();
        let mut window_context = window.to_async(cx);
        let task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(WORKSPACE_TAB_DRAG_SCROLL_INTERVAL)
                    .await;

                let pointer = window_context
                    .update(|window, app| {
                        if !app.has_active_drag() {
                            return None;
                        }
                        if !window.is_window_active() {
                            app.stop_active_drag(window);
                            return None;
                        }
                        Some(window.mouse_position())
                    })
                    .ok()
                    .flatten();
                let source_is_current = this
                    .update(cx, |tabs, _| {
                        tabs.drag_scroll_generation == generation
                            && tabs.drag_scroll_source.as_ref() == Some(&source)
                            && match &source {
                                super::WorkspaceTabDragSource::Tab(tab_id) => {
                                    tabs.projection.iter().any(|tab| &tab.id == tab_id)
                                }
                                super::WorkspaceTabDragSource::Group(group_id) => {
                                    tabs.groups.iter().any(|group| &group.id == group_id)
                                }
                            }
                    })
                    .unwrap_or(false);
                let Some(pointer) = pointer else {
                    break;
                };
                if !source_is_current {
                    let _ = window_context.update(|window, app| {
                        if app.has_active_drag() {
                            app.stop_active_drag(window);
                        }
                    });
                    break;
                }

                if let Some(next_offset) = super::next_tab_drag_scroll_offset(
                    pointer,
                    scroll_handle.bounds(),
                    scroll_handle.offset(),
                    scroll_handle.max_offset(),
                ) {
                    scroll_handle.set_offset(next_offset);
                    let _ = this.update(cx, |tabs, cx| {
                        if tabs.drag_scroll_generation == generation {
                            cx.notify();
                        }
                    });
                }
            }

            let _ = this.update(cx, |tabs, _| {
                if tabs.drag_scroll_generation == generation {
                    tabs.drag_scroll_task = None;
                    tabs.drag_scroll_source = None;
                }
            });
        });
        self.drag_scroll_task = Some(task);
    }

    /// 投放、取消或更换拖拽源时增加代次并丢弃唯一任务，避免迟到 tick 继续改滚动位置。
    ///
    /// @author kongweiguang
    fn stop_drag_edge_scroll(&mut self) {
        self.drag_scroll_generation = self.drag_scroll_generation.wrapping_add(1).max(1);
        self.drag_scroll_task = None;
        self.drag_scroll_source = None;
    }

    /// Root 在 Escape 或失焦时只取消此组件由类型化 Tab/Group drag source 登记的拖拽。
    /// 记录源身份而非重新校验 membership，确保标签在拖动中被关闭后仍能收回 GPUI drag。
    ///
    /// @author kongweiguang
    pub(crate) fn cancel_workspace_tab_drag(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.drag_scroll_source.is_none() {
            return false;
        }
        if !cx.has_active_drag() {
            self.stop_drag_edge_scroll();
            return false;
        }
        if !cx.stop_active_drag(window) {
            return false;
        }
        self.stop_drag_edge_scroll();
        cx.notify();
        true
    }

    /// 固定标签宽度避免内容长短造成抖动；溢出菜单按真实 viewport 测量显隐，新建按钮跟随尾部布局。
    /// 右键菜单捕获稳定身份；持久标签可拖动，共享 SFTP 临时视图固定在自己的工作台入口中。
    /// 标题用可聚焦 Tab 语义承载切换和拖动；未修饰的键盘激活复用同一事件，组合键交给 Root/系统。
    /// 每个 Tab 使用稳定焦点句柄；每次裸方向键/Home/End都用真实边界最小滚动，包括焦点已在目标的情况。
    /// 外框统一使用 main 的高度、圆角和内距，关闭命中区独立保留 22px，末项不会贴右缘。
    /// 拖放边界绑定到完整标签外框，关闭按钮仍独立接收点击。
    /// 标签本身不叠加组内横向留白；分隔线到首项的距离由工作台唯一外层提供。
    ///
    /// @author kongweiguang
    fn render_tab(
        &self,
        index: usize,
        theme: ThemeTokens,
        keyboard_targets: WorkspaceTabKeyboardTargets,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let tab = &self.projection[index];
        let tab_id = tab.id.clone();
        let tab_for_activation = tab_id.clone();
        let tab_for_keyboard = tab_id.clone();
        let drag_id = tab_id.clone();
        let drop_id = tab_id.clone();
        let drag_over_drop_id = drop_id.clone();
        let tab_for_hover = tab_id.clone();
        let tab_for_hover_exit = tab_id.clone();
        let title = self.display_title(index);
        let preview_title = title.clone();
        let full_title = tab.title.clone();
        let active = tab.active;
        let tab_kind = tab.kind.clone();
        let view = cx.entity().clone();
        let drag_validity_view = view.clone();
        let drag_start_view = view.clone();
        let drag_scroll_view = view.clone();
        let action_menu = tab_action_menu(view, tab_id.clone());
        let focus_handle = self
            .tab_focus_handles
            .get(&tab_id)
            .cloned()
            .unwrap_or_else(|| cx.focus_handle())
            .tab_stop(self.roving_focus_tab_id.as_ref() == Some(&tab_id))
            .tab_index(0);
        let tab_hitbox_selector = format!("workspace-tab-hitbox-{}", tab_id.as_str());
        let active_body_selector = format!("workspace-tab-active-body-{}", tab_id.as_str());
        let drop_bounds = Rc::new(RefCell::new(None));
        let drag_over_bounds = drop_bounds.clone();
        let on_drop_bounds = drop_bounds.clone();

        let label = div()
            .id(format!("workspace-tab-label-{}", tab_id.as_str()))
            .track_focus(&focus_handle)
            .role(gpui_kit::Role::Tab)
            .aria_label(full_title.clone())
            .aria_selected(active)
            .aria_keyshortcuts("Enter Space")
            .focus_visible(|style| style.bg(theme.hover))
            .h_full()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .on_click(cx.listener(move |tabs, _: &ClickEvent, window, cx| {
                tabs.roving_focus_tab_id = Some(tab_for_activation.clone());
                if let Some(focus_handle) = tabs.tab_focus_handles.get(&tab_for_activation) {
                    window.focus(focus_handle, cx);
                }
                cx.emit(WorkspaceTabEvent::Activate(tab_for_activation.clone()));
                cx.notify();
            }))
            .on_key_down(cx.listener(move |tabs, event: &KeyDownEvent, window, cx| {
                // Modified chords belong to Root/OS, especially Alt+Space; local tab keys stay unmodified.
                if event.keystroke.modifiers.modified() {
                    return;
                }
                let key = event.keystroke.key.to_ascii_lowercase();
                if matches!(key.as_str(), "enter" | "space" | " ") {
                    tabs.roving_focus_tab_id = Some(tab_for_keyboard.clone());
                    cx.emit(WorkspaceTabEvent::Activate(tab_for_keyboard.clone()));
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }

                let ordered_tab_ids = keyboard_targets
                    .iter()
                    .map(|(target_id, _)| target_id.clone())
                    .collect::<Vec<_>>();
                if let Some(target_index) = super::workspace_tab_keyboard_target_index(
                    &ordered_tab_ids,
                    &tab_for_keyboard,
                    &key,
                ) && let Some((target_id, target_focus)) = keyboard_targets.get(target_index)
                {
                    tabs.roving_focus_tab_id = Some(target_id.clone());
                    tabs.request_keyboard_tab_reveal(target_id.clone());
                    window.focus(target_focus, cx);
                    cx.notify();
                    cx.stop_propagation();
                }
            }));
        let label = if tab_kind != WorkspaceTabKind::SftpWorkbench {
            label
                .on_drag(drag_id, move |dragged_tab_id, _, window, app| {
                    let source = super::WorkspaceTabDragSource::Tab(dragged_tab_id.clone());
                    drag_start_view.update(app, |tabs, cx| {
                        tabs.start_drag_edge_scroll(source, window, cx);
                    });
                    app.new(|_| TabDragPreview {
                        title: preview_title.clone(),
                    })
                })
                .on_drag_move::<TabId>(move |event, window, app| {
                    let source = super::WorkspaceTabDragSource::Tab(event.drag(app).clone());
                    drag_scroll_view.update(app, |tabs, cx| {
                        tabs.start_drag_edge_scroll(source, window, cx);
                    });
                })
        } else {
            label
        };
        let label = label
            .tooltip(move |window, cx| Tooltip::new(full_title.clone()).build(window, cx))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(theme.text_sm)
                    .child(title),
            )
            .when(tab_kind != WorkspaceTabKind::Terminal, |label| {
                label.child(
                    div()
                        .flex_none()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(tab_kind_label(&tab_kind)),
                )
            });

        let close_tab_id = tab_id.clone();
        let close_label = crate::locale::text("关闭标签", "Close tab");
        let close_button_selector = format!("workspace-tab-close-{}", tab_id.as_str());
        let close_button = Button::new(close_button_selector.clone())
            .debug_selector(move || close_button_selector.clone())
            .compact()
            .ghost()
            .icon(IconName::Close)
            .w(px(22.))
            .h(px(22.))
            .accessibility_label(close_label.clone())
            .tooltip(close_label)
            .focus_ring(true)
            .tab_stop(true)
            .on_click(cx.listener(move |tabs, _: &ClickEvent, window, cx| {
                tabs.request_close_tab(close_tab_id.clone(), window, cx);
            }));
        let tab = div()
            .id(format!(
                "workspace-tab-{}",
                self.projection[index].id.as_str()
            ))
            .debug_selector(move || tab_hitbox_selector.clone())
            .relative()
            .w(px(180.))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(WORKSPACE_TAB_CONTENT_GAP))
            .h(px(WORKSPACE_TAB_HEIGHT))
            .px(px(WORKSPACE_TAB_PADDING_X))
            .rounded(theme.radius)
            .border_1()
            .border_color(if active {
                theme.background.opacity(0.)
            } else {
                theme.materials.chrome
            })
            .when(!active, |tab| tab.bg(theme.materials.panel.opacity(0.46)))
            .text_color(theme.foreground)
            .when(!active, |tab| tab.hover(|style| style.bg(theme.hover)))
            .on_prepaint(move |bounds, _, _| {
                *drop_bounds.borrow_mut() = Some(bounds);
            })
            .drag_over::<kerminal_domain::identity::TabId>(move |style, dragged, window, app| {
                if dragged == &drag_over_drop_id {
                    return style;
                }
                let tabs = drag_validity_view.read(app);
                if !tabs.projection.iter().any(|tab| &tab.id == dragged)
                    || !tabs
                        .projection
                        .iter()
                        .any(|tab| tab.id == drag_over_drop_id)
                {
                    return style;
                }
                let Some(bounds) = *drag_over_bounds.borrow() else {
                    return style;
                };
                match super::tab_drop_placement(window.mouse_position(), bounds) {
                    super::TabDropPlacement::Before => {
                        style.border_l_2().border_color(theme.accent)
                    }
                    super::TabDropPlacement::After => style.border_r_2().border_color(theme.accent),
                }
            })
            .on_drop(cx.listener(
                move |tabs, dragged: &kerminal_domain::identity::TabId, window, cx| {
                    tabs.stop_drag_edge_scroll();
                    // 异步关闭可能先于最后一次滚动 tick；释放时重新核对双方，过期拖动静默收尾。
                    if dragged == &drop_id
                        || !tabs.projection.iter().any(|tab| &tab.id == dragged)
                        || !tabs.projection.iter().any(|tab| tab.id == drop_id)
                    {
                        return;
                    }
                    let Some(bounds) = *on_drop_bounds.borrow() else {
                        return;
                    };
                    cx.emit(tab_drop_event(
                        dragged.clone(),
                        drop_id.clone(),
                        super::tab_drop_placement(window.mouse_position(), bounds),
                    ));
                },
            ))
            .on_mouse_move(cx.listener(move |tabs, _, _, cx| {
                if tabs.hovered_tab_id.as_ref() != Some(&tab_for_hover) {
                    tabs.hovered_tab_id = Some(tab_for_hover.clone());
                    cx.notify();
                }
            }))
            .on_mouse_exit(cx.listener(move |tabs, _, _, cx| {
                if tabs.hovered_tab_id.as_ref() == Some(&tab_for_hover_exit) {
                    tabs.hovered_tab_id = None;
                    cx.notify();
                }
            }));
        let tab = if active {
            tab.child(
                div()
                    .absolute()
                    .debug_selector(move || active_body_selector.clone())
                    .inset_0()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background),
            )
        } else {
            tab
        };
        tab.child(label)
            .child(close_button)
            .context_menu(move |menu, window, cx| action_menu(menu, window, cx))
            .into_any_element()
    }

    /// 真实分组只保留色标与可折叠标题，次要操作统一由右键承载，不额外占用标签左端。
    /// 未分组命中区挂在末尾组区域而非标题，横向剩余空间可整片投放；父级单一回退 handler 仍仅接收 TabId。
    /// 右键仍可打开同一菜单；组拖放继续以原始组 ID 为目标，不重建标签或会话。
    /// 标题继续使用紧凑 Button 的原生焦点与点击语义，不改标签高度或右键目标。
    /// 活动标签定位只读取该帧真实子项边界，保持组标题、拖放区域与用户滚动不变。
    /// 拖组按组头左右半区提交 before-group，拖标签落在组头时定位到组内末项之后。
    /// 折叠标题持续显示成员数和活动标记，颜色只来自当前主题下的组色投影。
    ///
    /// @author kongweiguang
    fn render_group(
        &self,
        group_id: Option<String>,
        indices: Vec<usize>,
        theme: ThemeTokens,
        keyboard_targets: WorkspaceTabKeyboardTargets,
        options: WorkspaceTabGroupRenderOptions,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let WorkspaceTabGroupRenderOptions {
            show_header,
            is_ungrouped_drop_target,
            tab_reveal,
        } = options;
        let group = group_id
            .as_ref()
            .and_then(|id| self.groups.iter().find(|group| &group.id == id));
        let collapsed = group.is_some_and(|group| group.collapsed);
        let group_name = workspace_group_title(group_id.as_deref(), group);
        let id = group_id.clone();
        let group_is_populated = !indices.is_empty();
        let group_has_active_tab = indices.iter().any(|index| self.projection[*index].active);
        let group_preview_title = group_name.clone();
        let entity = cx.entity().clone();
        let tint = tab_group_color_tint(group.and_then(|group| group.color.as_ref()), theme);
        let group_member_count = indices.len();
        let group_last_tab_id = indices
            .last()
            .map(|index| self.projection[*index].id.clone());

        if !show_header {
            let mut tabs = Vec::with_capacity(indices.len().saturating_mul(2));
            let mut reveal_index = None;
            let mut previous_index = None;
            for index in indices {
                if let Some(previous) = previous_index {
                    tabs.push(tab_separator(
                        &self.projection[previous],
                        &self.projection[index],
                        self.hovered_tab_id.as_ref(),
                        theme,
                    ));
                }
                if tab_reveal
                    .as_ref()
                    .is_some_and(|measurement| measurement.tab_id == self.projection[index].id)
                {
                    reveal_index = Some(tabs.len());
                }
                tabs.push(self.render_tab(index, theme, keyboard_targets.clone(), cx));
                previous_index = Some(index);
            }
            let region = div().flex().items_center().gap_0().children(tabs);
            let region = match (reveal_index, tab_reveal) {
                (Some(index), Some(TabRevealMeasurement { bounds, .. })) => region
                    .on_children_prepainted(move |children, _, _| {
                        *bounds.borrow_mut() = children.get(index).cloned();
                    }),
                _ => region,
            };
            return region
                .id(format!(
                    "workspace-tab-group-region-{}",
                    id.as_deref().unwrap_or("ungrouped")
                ))
                .into_any_element();
        }

        let mut header = div()
            .id(format!(
                "workspace-tab-group-{}",
                group_id.as_deref().unwrap_or("ungrouped")
            ))
            .h(px(30.))
            .px_2()
            .flex()
            .items_center()
            .gap_1()
            .rounded_lg()
            .bg(if group_id.is_some() {
                tint.opacity(0.14)
            } else {
                theme.sidebar
            })
            .text_color(theme.foreground);

        if let Some(drag_group_id) = group_id.clone().filter(|_| group_is_populated) {
            let group_header_id = drag_group_id.clone();
            let drop_group_id = drag_group_id.clone();
            let preview_title = group_preview_title.clone();
            let group_header_bounds = Rc::new(RefCell::new(None));
            let drag_over_bounds = group_header_bounds.clone();
            let on_drop_bounds = group_header_bounds.clone();
            let drag_start_view = entity.clone();
            let drag_scroll_view = entity.clone();
            let drag_validity_view = entity.clone();
            let ordered_group_ids = self
                .groups
                .iter()
                .map(|group| group.id.clone())
                .collect::<Vec<_>>();
            let drop_ordered_group_ids = ordered_group_ids.clone();
            header = header
                .on_prepaint(move |bounds, _, _| {
                    *group_header_bounds.borrow_mut() = Some(bounds);
                })
                .on_drag(
                    GroupDragPayload(drag_group_id),
                    move |dragged_group, _, window, app| {
                        let source = super::WorkspaceTabDragSource::Group(dragged_group.0.clone());
                        drag_start_view.update(app, |tabs, cx| {
                            tabs.start_drag_edge_scroll(source, window, cx);
                        });
                        app.new(|_| GroupDragPreview {
                            title: preview_title.clone(),
                        })
                    },
                )
                .on_drag_move::<GroupDragPayload>(move |event, window, app| {
                    let source = super::WorkspaceTabDragSource::Group(event.drag(app).0.clone());
                    drag_scroll_view.update(app, |tabs, cx| {
                        tabs.start_drag_edge_scroll(source, window, cx);
                    });
                })
                .drag_over::<GroupDragPayload>(move |style, dragged, window, app| {
                    if dragged.0 == group_header_id {
                        return style;
                    }
                    let tabs = drag_validity_view.read(app);
                    if !tabs.groups.iter().any(|group| group.id == dragged.0)
                        || !tabs.groups.iter().any(|group| group.id == group_header_id)
                    {
                        return style;
                    }
                    let Some(bounds) = *drag_over_bounds.borrow() else {
                        return style;
                    };
                    match super::tab_drop_placement(window.mouse_position(), bounds) {
                        super::TabDropPlacement::Before => {
                            style.border_l_2().border_color(theme.accent)
                        }
                        super::TabDropPlacement::After => {
                            style.border_r_2().border_color(theme.accent)
                        }
                    }
                })
                .on_drop(
                    cx.listener(move |tabs, dragged: &GroupDragPayload, window, cx| {
                        tabs.stop_drag_edge_scroll();
                        if dragged.0 != drop_group_id
                            && let Some(bounds) = *on_drop_bounds.borrow()
                            && let Some(before_group_id) = super::group_drop_before_id(
                                &drop_ordered_group_ids,
                                &drop_group_id,
                                super::tab_drop_placement(window.mouse_position(), bounds),
                            )
                        {
                            cx.emit(WorkspaceTabEvent::MoveGroup {
                                group_id: dragged.0.clone(),
                                before_group_id,
                            });
                        }
                    }),
                );
        }
        header = header.child(div().w(px(3.)).h(px(18.)).rounded_full().bg(tint));

        let header = if let Some(group) = group {
            let target_group_id = group.id.clone();
            let drag_target_group_id = target_group_id.clone();
            let target_group_last_tab_id = group_last_tab_id.clone();
            let toggle_group_id = group.id.clone();
            let title_hint = crate::locale::text(
                "单击折叠或展开；右键打开分组操作",
                "Click to collapse or expand; right-click for group actions",
            );
            let group_accessible_name = if group_has_active_tab {
                crate::locale::text(
                    format!(
                        "{}，{} 个标签，{}，包含活动标签",
                        group.title,
                        group_member_count,
                        if collapsed { "已折叠" } else { "已展开" }
                    ),
                    format!(
                        "{}, {} tabs, {}, contains active tab",
                        group.title,
                        group_member_count,
                        if collapsed { "collapsed" } else { "expanded" }
                    ),
                )
            } else {
                crate::locale::text(
                    format!(
                        "{}，{} 个标签，{}",
                        group.title,
                        group_member_count,
                        if collapsed { "已折叠" } else { "已展开" }
                    ),
                    format!(
                        "{}, {} tabs, {}",
                        group.title,
                        group_member_count,
                        if collapsed { "collapsed" } else { "expanded" }
                    ),
                )
            };
            let title = Button::new(format!("workspace-group-title-{}", group.id))
                .compact()
                .ghost()
                .label(group.title.clone())
                .accessibility_label(group_accessible_name)
                .tooltip(title_hint)
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    cx.emit(WorkspaceTabEvent::SetGroupCollapsed {
                        group_id: toggle_group_id.clone(),
                        collapsed: !collapsed,
                    });
                }));
            let header = header.child(title).when(collapsed, |header| {
                let header = if group_has_active_tab {
                    header.child(
                        div()
                            .w(px(6.))
                            .h(px(6.))
                            .flex_none()
                            .rounded_full()
                            .bg(theme.accent),
                    )
                } else {
                    header
                };
                header.child(
                    div()
                        .px_1()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .flex_none()
                        .child(group_member_count.to_string()),
                )
            });

            let drag_validity_view = entity.clone();
            let header = header
                .drag_over::<kerminal_domain::identity::TabId>(move |style, dragged, _, app| {
                    let tabs = drag_validity_view.read(app);
                    if tabs.projection.iter().any(|tab| &tab.id == dragged)
                        && tabs
                            .groups
                            .iter()
                            .any(|group| group.id == drag_target_group_id)
                    {
                        style.border_2().border_color(theme.accent)
                    } else {
                        style
                    }
                })
                .on_drop(cx.listener(
                    move |tabs, dragged: &kerminal_domain::identity::TabId, _, cx| {
                        tabs.stop_drag_edge_scroll();
                        if let Some(last_tab_id) = target_group_last_tab_id.as_ref() {
                            if dragged != last_tab_id {
                                cx.emit(tab_drop_event(
                                    dragged.clone(),
                                    last_tab_id.clone(),
                                    super::TabDropPlacement::After,
                                ));
                            }
                        } else {
                            cx.emit(WorkspaceTabEvent::MoveToGroup {
                                tab_id: dragged.clone(),
                                group_id: Some(target_group_id.clone()),
                            });
                        }
                    },
                ));
            let action_menu = group_action_menu(entity.clone(), group.id.clone());
            header
                .context_menu(move |menu, window, cx| action_menu(menu, window, cx))
                .into_any_element()
        } else {
            header
                .child(div().flex_1().child(group_name))
                .into_any_element()
        };

        let mut tab_row = Vec::with_capacity(indices.len().saturating_mul(2));
        let mut reveal_index = None;
        let mut previous_index = None;
        for index in indices {
            if let Some(previous) = previous_index {
                tab_row.push(tab_separator(
                    &self.projection[previous],
                    &self.projection[index],
                    self.hovered_tab_id.as_ref(),
                    theme,
                ));
            }
            if tab_reveal
                .as_ref()
                .is_some_and(|measurement| measurement.tab_id == self.projection[index].id)
            {
                reveal_index = Some(tab_row.len());
            }
            tab_row.push(self.render_tab(index, theme, keyboard_targets.clone(), cx));
            previous_index = Some(index);
        }
        let tab_row = div().flex().items_center().gap_0().children(tab_row);
        let tab_row = match (reveal_index, tab_reveal) {
            (Some(index), Some(TabRevealMeasurement { bounds, .. })) => tab_row
                .on_children_prepainted(move |children, _, _| {
                    *bounds.borrow_mut() = children.get(index).cloned();
                }),
            _ => tab_row,
        };
        let mut region = div()
            .id(format!(
                "workspace-tab-group-region-{}",
                id.as_deref().unwrap_or("ungrouped")
            ))
            .when(group_id.is_some(), |region| {
                region.border_b_2().border_color(tint)
            })
            .flex()
            .items_center()
            .gap_1();
        if is_ungrouped_drop_target {
            // Header 和 tab 可有自己的点击/重排行为；未分组移出意图只在本父区域兜底一次。
            let drag_validity_view = entity.clone();
            region = region
                .flex_1()
                .min_w(px(72.))
                .h_full()
                .drag_over::<kerminal_domain::identity::TabId>(move |style, dragged, _, app| {
                    if drag_validity_view
                        .read(app)
                        .projection
                        .iter()
                        .any(|tab| &tab.id == dragged)
                    {
                        style.border_1().border_color(theme.accent)
                    } else {
                        style
                    }
                })
                .on_drop(cx.listener(
                    move |tabs, dragged: &kerminal_domain::identity::TabId, _, cx| {
                        tabs.stop_drag_edge_scroll();
                        cx.emit(ungrouped_tab_drop_event(dragged.clone()));
                    },
                ));
        }
        region
            .child(header)
            .when(!collapsed, |container| container.child(tab_row))
            .into_any_element()
    }
}

impl Render for WorkspaceTabs {
    /// 标签栏固定 36px；名称编辑和单行错误都留在原行，避免额外反馈被标题栏裁剪。
    /// 错误态关闭 Input 焦点环以保留错误边框，并让输入/错误摘要在窄栏收缩，保存与取消控件保持可见。
    /// 只有真实溢出才显示列表入口；无溢出时 + 位于最后标签后，溢出时固定在列表入口右侧。
    /// 滚动轨道撑满标题栏并居中子项，使尾随 + 与轨道外的列表/新建按钮使用同一条垂直中线。
    /// 方向键/Home/End只在当前可见标签间移动焦点，并且仅显式移动时依真实 bounds 最小滚动；普通重绘保留手动位置。
    /// 拖拽边缘滚动由单个短间隔任务维持，松手、取消、失焦或源失效即退出。
    /// 加号单击发布本地新建意图，右键菜单在打开时读取左栏同源主机快照并保留分组。
    /// 列表打开时读取完整投影，折叠组和屏外标签仍可按稳定 ID 激活，长列表沿用原生菜单滚动。
    /// Kit Input 没有 description builder，完整错误仍并入可访问名称和保存按钮提示。
    /// 空白未分组区域只在活动拖拽时扩张到组列末尾剩余空间；静止布局不显示独立未分组标题。
    /// 普通态也按可见组头与固定标签预算轨道宽度，确保 ScrollHandle 测得完整内容；拖拽态仅额外计入末尾 72px 投放区。
    /// 拖放反馈只在 GPUI payload 与当前稳定 ID 投影均有效时显示；边缘滚动任务仅暂存来源和代次。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.reconcile_tab_focus_handles(cx);
        let theme = ThemeTokens::current(window, cx).for_workbench();
        let scroll_max = self.scroll_handle.max_offset().x;
        let picker_visible = self.show_tab_picker;
        let pending_tab_reveal = self.pending_tab_reveal.clone();
        let tab_reveal_bounds = Rc::new(RefCell::new(None));
        let tabs_entity = cx.entity().clone();
        let scroll_handle = self.scroll_handle.clone();
        let callback_bounds = tab_reveal_bounds.clone();
        let pending_for_callback = pending_tab_reveal.clone();
        window.on_next_frame(move |window, app| {
            // 预绘制得到真实标签边界；若偏移已变化则视为用户手动滚动并放弃旧请求。
            let measured_max_offset = scroll_handle.max_offset().x;
            let max_changed = measured_max_offset != scroll_max;
            let next_picker_visible =
                workspace_tab_picker_should_show(measured_max_offset, picker_visible);
            let picker_changed = next_picker_visible != picker_visible;
            if max_changed || picker_changed || pending_for_callback.is_some() {
                let measured_tab = *callback_bounds.borrow();
                let viewport = scroll_handle.bounds();
                let current_offset = scroll_handle.offset();
                let may_reveal = pending_for_callback
                    .as_ref()
                    .is_some_and(|pending| pending.expected_offset == current_offset);
                let next_offset = measured_tab.map(|tab| {
                    super::minimal_active_tab_reveal_offset(current_offset, viewport, tab)
                });
                tabs_entity.update(app, |tabs, cx| {
                    let picker_state_is_current = tabs.show_tab_picker == picker_visible;
                    let picker_was_updated = picker_state_is_current && picker_changed;
                    if picker_was_updated {
                        tabs.show_tab_picker = next_picker_visible;
                    }
                    let pending_is_current = pending_for_callback.as_ref().is_some_and(|pending| {
                        tabs.pending_tab_reveal.as_ref().is_some_and(|current| {
                            current.tab_id == pending.tab_id
                                && current.generation == pending.generation
                        })
                    });
                    if pending_is_current {
                        let target_is_current =
                            pending_for_callback.as_ref().is_some_and(|pending| {
                                match pending.reason {
                                    super::TabRevealReason::ActiveSelection => tabs
                                        .projection
                                        .iter()
                                        .any(|tab| tab.active && tab.id == pending.tab_id),
                                    super::TabRevealReason::KeyboardFocus => {
                                        let target_is_visible = tabs
                                            .projection
                                            .iter()
                                            .find(|tab| tab.id == pending.tab_id)
                                            .is_some_and(|tab| {
                                                !tab.group_id.as_ref().is_some_and(|group_id| {
                                                    tabs.groups.iter().any(|group| {
                                                        &group.id == group_id && group.collapsed
                                                    })
                                                })
                                            });
                                        target_is_visible
                                            && tabs.roving_focus_tab_id.as_ref()
                                                == Some(&pending.tab_id)
                                            && tabs
                                                .tab_focus_handles
                                                .get(&pending.tab_id)
                                                .is_some_and(|focus_handle| {
                                                    focus_handle.is_focused(window)
                                                })
                                    }
                                }
                            });
                        tabs.pending_tab_reveal = None;
                        if may_reveal
                            && target_is_current
                            && let Some(next_offset) = next_offset
                            && next_offset != current_offset
                        {
                            scroll_handle.set_offset(next_offset);
                            cx.notify();
                        }
                    }
                    if max_changed || picker_was_updated {
                        cx.notify();
                    }
                });
            }
        });
        let groups = self.grouped_indices();
        let visible_tab_ids = groups
            .iter()
            .filter(|(group_id, _)| {
                !group_id.as_ref().is_some_and(|group_id| {
                    self.groups
                        .iter()
                        .find(|group| &group.id == group_id)
                        .is_some_and(|group| group.collapsed)
                })
            })
            .flat_map(|(_, indices)| {
                indices
                    .iter()
                    .map(|index| self.projection[*index].id.clone())
            })
            .collect::<Vec<_>>();
        let old_roving_tab_was_focused = self
            .roving_focus_tab_id
            .as_ref()
            .and_then(|tab_id| self.tab_focus_handles.get(tab_id))
            .is_some_and(|focus_handle| focus_handle.is_focused(window));
        if !self
            .roving_focus_tab_id
            .as_ref()
            .is_some_and(|tab_id| visible_tab_ids.contains(tab_id))
        {
            self.roving_focus_tab_id = visible_tab_ids
                .iter()
                .find(|tab_id| {
                    self.projection
                        .iter()
                        .any(|tab| &tab.id == *tab_id && tab.active)
                })
                .or_else(|| visible_tab_ids.first())
                .cloned();
            if old_roving_tab_was_focused
                && let Some(focus_handle) = self
                    .roving_focus_tab_id
                    .as_ref()
                    .and_then(|tab_id| self.tab_focus_handles.get(tab_id))
            {
                window.focus(focus_handle, cx);
            }
        }
        let keyboard_targets = Rc::new(
            visible_tab_ids
                .iter()
                .filter_map(|tab_id| {
                    self.tab_focus_handles
                        .get(tab_id)
                        .map(|focus_handle| (tab_id.clone(), focus_handle.clone()))
                })
                .collect::<Vec<_>>(),
        );
        let has_groups = !self.groups.is_empty();
        let has_active_drag = cx.has_active_drag();
        let has_ungrouped_drop_target =
            should_enable_ungrouped_drop_target(has_groups, has_active_drag);
        let mut sections = Vec::with_capacity(groups.len());
        let mut track_sections = Vec::with_capacity(groups.len());
        for (group_id, indices) in groups {
            let group = group_id
                .as_ref()
                .and_then(|id| self.groups.iter().find(|group| &group.id == id));
            let collapsed = group.is_some_and(|group| group.collapsed);
            let show_header = should_show_workspace_group_header(
                group_id.as_deref(),
                has_groups,
                !indices.is_empty(),
                has_active_drag,
            );
            let is_ungrouped_drop_target = group_id.is_none() && has_ungrouped_drop_target;
            track_sections.push(WorkspaceTabTrackSection {
                header_text_units: show_header.then(|| {
                    workspace_group_title(group_id.as_deref(), group)
                        .encode_utf16()
                        .count()
                }),
                collapsed_header: show_header && collapsed,
                visible_tab_count: if collapsed { 0 } else { indices.len() },
                is_ungrouped_drop_target,
            });
            let tab_reveal = pending_tab_reveal
                .as_ref()
                .filter(|pending| {
                    indices
                        .iter()
                        .any(|index| self.projection[*index].id == pending.tab_id)
                })
                .map(|pending| TabRevealMeasurement {
                    tab_id: pending.tab_id.clone(),
                    bounds: tab_reveal_bounds.clone(),
                });
            sections.push(self.render_group(
                group_id.clone(),
                indices,
                theme,
                keyboard_targets.clone(),
                WorkspaceTabGroupRenderOptions {
                    show_header,
                    is_ungrouped_drop_target,
                    tab_reveal,
                },
                cx,
            ));
        }
        let track_width = workspace_tab_track_width_budget(&track_sections);
        let tab_bar = if let Some(input) = self.name_input.as_ref() {
            let input_label = match self.name_edit_target.as_ref() {
                Some(super::WorkspaceNameEditTarget::CreateGroup { .. }) => {
                    crate::locale::text("分组名称", "Group name")
                }
                Some(super::WorkspaceNameEditTarget::RenameTab { .. }) => {
                    crate::locale::text("标签名称", "Tab name")
                }
                Some(super::WorkspaceNameEditTarget::RenameGroup { .. }) => {
                    crate::locale::text("分组名称", "Group name")
                }
                None => crate::locale::text("名称", "Name"),
            };
            let input_accessible_label = self
                .name_error
                .as_ref()
                .map(|error| format!("{input_label}: {error}"))
                .unwrap_or_else(|| input_label.to_string());
            let has_error = self.name_error.is_some();
            let mut input = gpui_kit::component::input::Input::new(input)
                .id("workspace-name-edit-input")
                .aria_label(input_accessible_label)
                .flex_1()
                .flex_shrink_1()
                .min_w(px(if has_error { 48. } else { 80. }))
                .h(px(28.));
            if has_error {
                input = input.border_1().border_color(theme.error).focus_ring(false);
            }
            let save_button = Button::new("workspace-name-save")
                .compact()
                .flex_none()
                .label(crate::locale::text("保存", "Save"));
            // 完整错误保留在提示中；行内仅展示可收缩摘要，避免挤走保存/取消按钮。
            let save_button = if let Some(error) = self.name_error.clone() {
                save_button.tooltip(error)
            } else {
                save_button
            };
            let key_snapshot = self.name_key_snapshot.clone();
            div()
                .id("workspace-name-edit-row")
                .w_full()
                .h(px(36.))
                .px_2()
                .flex()
                .items_center()
                .gap_2()
                .bg(theme.materials.chrome)
                .child(input)
                .when_some(self.name_error.clone(), |row, error| {
                    row.child(
                        div()
                            .id("workspace-name-edit-error")
                            .min_w(px(0.))
                            .max_w(px(180.))
                            .flex_shrink_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(theme.text_xs)
                            .text_color(theme.error)
                            .child(error),
                    )
                })
                .on_key_down(cx.listener(move |tabs, event: &KeyDownEvent, _, cx| {
                    let Some(action) = super::take_workspace_name_edit_key_action(
                        &key_snapshot,
                        tabs.name_edit_generation,
                        &event.keystroke.key,
                    ) else {
                        return;
                    };
                    match action {
                        super::WorkspaceNameEditKeyAction::Submit => tabs.submit_name_edit(cx),
                        super::WorkspaceNameEditKeyAction::Cancel => tabs.cancel_name_edit(cx),
                    }
                    cx.stop_propagation();
                }))
                .child(
                    save_button.on_click(cx.listener(|tabs, _: &ClickEvent, _, cx| {
                        tabs.submit_name_edit(cx);
                    })),
                )
                .child(
                    Button::new("workspace-name-cancel")
                        .compact()
                        .flex_none()
                        .label(crate::locale::text("取消", "Cancel"))
                        .on_click(cx.listener(|tabs, _: &ClickEvent, _, cx| {
                            tabs.cancel_name_edit(cx);
                        })),
                )
                .into_any_element()
        } else {
            let tabs_entity = cx.entity().clone();
            let create_navigation = self.host_navigation.clone();
            let create_search_input = self.ensure_create_target_search_input(window, cx);
            let create_focus_handle = create_search_input.read(cx).focus_handle(cx);
            let (create_groups, create_loading, create_status, create_options) = self
                .host_navigation
                .as_ref()
                .map(|navigation| {
                    let navigation = navigation.read(cx);
                    (
                        navigation.split_target_groups(),
                        navigation.split_target_loading(),
                        navigation.split_target_status(),
                        navigation.split_target_options(""),
                    )
                })
                .unwrap_or_default();
            let create_theme = ThemeTokens::current(window, cx).for_workbench();
            let create_query = self.create_target_query.clone();
            let target_list_tabs = tabs_entity.clone();
            let create_content =
                move |_: &mut gpui_kit::component::popover::PopoverState,
                      _: &mut Window,
                      _: &mut Context<gpui_kit::component::popover::PopoverState>| {
                    workspace_create_target_list(WorkspaceCreateTargetListProjection {
                        tabs: target_list_tabs.clone(),
                        navigation: create_navigation.clone(),
                        search_input: create_search_input.clone(),
                        query: create_query.clone(),
                        options: create_options.clone(),
                        groups: create_groups.clone(),
                        loading: create_loading,
                        directory_status: create_status.clone(),
                        theme: create_theme,
                    })
                };
            let open_tabs = tabs_entity.clone();
            let create_terminal_button = host_picker_popover(HostPickerPopoverConfig {
                id: "workspace-tabs-create-terminal-popover".into(),
                trigger: Button::new("workspace-tabs-create-terminal")
                    .debug_selector(|| "workspace-tabs-create-terminal".to_owned())
                    .compact()
                    .ghost()
                    .icon(IconName::Plus)
                    .w(px(WORKSPACE_TAB_BUTTON_WIDTH))
                    .h(px(WORKSPACE_TAB_BUTTON_WIDTH))
                    .flex_none()
                    .accessibility_label(crate::locale::text(
                        "新建本地终端，右键选择主机",
                        "New local terminal; right-click to choose a host",
                    ))
                    .tooltip(crate::locale::text(
                        "单击新建本地终端，右击选择主机",
                        "Click to open a local terminal; right-click to choose a host",
                    ))
                    .tab_stop(true)
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                        cx.emit(WorkspaceTabsCreateEvent::Local);
                    })),
                mouse_button: MouseButton::Right,
                open: self.create_target_picker_open,
                trigger_style: StyleRefinement::default()
                    .w(px(WORKSPACE_TAB_BUTTON_WIDTH))
                    .h(px(WORKSPACE_TAB_BUTTON_WIDTH))
                    .flex_none(),
                focus_handle: Some(create_focus_handle),
                on_open_change: move |open: &bool, window: &mut Window, app: &mut gpui_kit::App| {
                    open_tabs.update(app, |tabs, cx| {
                        tabs.set_create_target_picker_open(*open, window, cx);
                    });
                },
                content: create_content,
            });
            let viewport = workspace_tabs_scroll_viewport().track_scroll(&self.scroll_handle);
            let tab_bar = if self.show_tab_picker {
                let tab_list_label = crate::locale::text("标签列表", "Tab list");
                let picker_entity = tabs_entity;
                let picker = Button::new("workspace-tabs-picker")
                    .compact()
                    .ghost()
                    .icon(IconName::ChevronDown)
                    .w(px(WORKSPACE_TAB_BUTTON_WIDTH))
                    .h(px(WORKSPACE_TAB_BUTTON_WIDTH))
                    .flex_none()
                    .accessibility_label(tab_list_label.clone())
                    .tooltip(tab_list_label)
                    .tab_stop(true)
                    .dropdown_menu(move |mut menu, _, app| {
                        for (id, title, active) in picker_entity.read(app).tab_picker_entries() {
                            menu = menu.item(
                                event_item(
                                    picker_entity.clone(),
                                    title,
                                    WorkspaceTabEvent::Activate(id),
                                )
                                .checked(active),
                            );
                        }
                        menu.scrollable(true)
                    });
                let track = div()
                    .flex()
                    .h_full()
                    .items_center()
                    .gap_1()
                    .min_w_full()
                    .w(px(track_width))
                    .children(sections);
                div()
                    .w_full()
                    .min_w(px(0.))
                    .h(px(36.))
                    .pr_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .overflow_hidden()
                    .bg(theme.materials.chrome)
                    .child(viewport.child(track))
                    .child(picker)
                    .child(create_terminal_button)
            } else {
                let has_sections = !sections.is_empty();
                let track_width_with_create_button = track_width
                    + WORKSPACE_TAB_BUTTON_WIDTH
                    + if has_sections {
                        WORKSPACE_TAB_GAP * 2.
                    } else {
                        WORKSPACE_TAB_GAP
                    };
                let trailing_drop_view = tabs_entity.clone();
                let trailing_drag_space = div()
                    .id("workspace-tabs-trailing-drag-space")
                    .debug_selector(|| "workspace-tabs-trailing-drag-space".to_owned())
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .when(!has_active_drag, |space| {
                        space.window_control_area(WindowControlArea::Drag)
                    })
                    .drag_over::<TabId>(move |style, dragged, _, app| {
                        if trailing_drop_view
                            .read(app)
                            .projection
                            .iter()
                            .any(|tab| &tab.id == dragged)
                        {
                            style.border_l_2().border_color(theme.accent)
                        } else {
                            style
                        }
                    })
                    // 尾部命中只发一个带稳定 TabId 的意图，让 controller 原子完成解组和追加。
                    .on_drop(cx.listener(move |tabs, dragged: &TabId, _, cx| {
                        tabs.stop_drag_edge_scroll();
                        if let Some(event) = trailing_tab_drop_event(dragged, &tabs.projection) {
                            cx.emit(event);
                        }
                    }));
                let track = div()
                    .flex()
                    .h_full()
                    .items_center()
                    .gap_1()
                    .min_w_full()
                    .w(px(track_width_with_create_button))
                    .children(sections)
                    .child(create_terminal_button)
                    .child(trailing_drag_space);
                div()
                    .w_full()
                    .min_w(px(0.))
                    .h(px(36.))
                    .pr_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .overflow_hidden()
                    .bg(theme.materials.chrome)
                    .child(viewport.child(track))
            };
            tab_bar.into_any_element()
        };
        div()
            .id("workspace-tabs-view")
            .debug_selector(|| "workspace-tabs-view".to_owned())
            .w_full()
            .min_w(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .gap_0()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(tab_bar)
            .into_any_element()
    }
}
