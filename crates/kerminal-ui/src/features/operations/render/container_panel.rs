//! 跟随终端宿主的容器列表、详情导航与操作入口。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, ClickEvent, Context, InteractiveElement as _, IntoElement, SharedString,
    StatefulInteractiveElement as _,
    base::Selectable as _,
    component::{
        Disableable, Sizable as _, Size,
        button::{Button, ButtonVariants},
        input::Input,
        menu::{DropdownMenu as _, PopupMenuItem},
        switch::Switch,
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px, relative,
};
use kerminal_domain::{
    identity::HostId,
    operations::{
        ComposeAction, ComposeProjectTarget, ContainerAction, ContainerRuntime, ContainerStats,
        ContainerStatus, ContainerSummary, ContainerTarget,
    },
};

use crate::{theme::ThemeTokens, ui_button::icon_button};

use super::super::{
    ContainerPanelContext, ContainerPanelUnavailableReason, ContainerReadKind, OperationsEvent,
    OperationsView,
    state::{
        ContainerMutationRequest, ContainerProjectGroup, ContainerStatusFilter, filter_containers,
        group_container_projects_with_retained,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContainerRowActivation {
    Inspect,
    Enter,
}

/// 列表首击和空格进入详情；仅运行中容器的 Enter 沿用直接进入终端的快捷入口。
/// 鼠标双击不另设终端入口：首击切页后第二击会落到新页面，无法稳定命中同一行。
///
/// @author kongweiguang
fn container_row_activation(
    project_detail: bool,
    event: &ClickEvent,
    status: ContainerStatus,
) -> ContainerRowActivation {
    if project_detail {
        return ContainerRowActivation::Inspect;
    }
    if matches!(event, ClickEvent::Keyboard(click) if click.button == gpui_kit::KeyboardButton::Enter)
        && status == ContainerStatus::Running
    {
        return ContainerRowActivation::Enter;
    }
    ContainerRowActivation::Inspect
}

impl OperationsView {
    /// includeStopped 是立即生效的列表查询条件；只在值变化后重读，避免重复开请求。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn set_host_container_include_stopped(
        &mut self,
        include_stopped: bool,
        cx: &mut Context<Self>,
    ) {
        if self.container_host.set_include_stopped(include_stopped) {
            self.load_containers(cx);
        }
    }

    /// Pin 转换只由 Root 修改 workspace，当前 ticket 将菜单动作固定到原容器和会话上下文。
    ///
    /// @author kongweiguang
    pub(super) fn request_container_pin(
        &mut self,
        container: ContainerSummary,
        ticket: &super::super::container_panel::ContainerPanelActionTicket,
        cx: &mut Context<Self>,
    ) {
        let target = container.target.clone();
        if ticket.target.as_ref() != Some(&target)
            || !self.container_action_ticket_is_current(ticket, Some(&target))
        {
            return;
        }
        match self.container_panel_context.as_ref() {
            Some(
                ContainerPanelContext::Host { .. } | ContainerPanelContext::WorkspaceHost { .. },
            ) => {}
            None if !self.section_selection.is_fixed() => {}
            _ => return,
        }

        self.container_host.select_container_row(target.clone());
        cx.emit(OperationsEvent::ContainerPinRequested {
            context: ticket.context.clone(),
            target,
            container: Box::new(container),
        });
        cx.notify();
    }

    /// Inspector 入口把显式 tab、完整目标与来源项目一起固定，再发起受 Root gate 保护的读取。
    /// 项目容器页签进入时保留来源，返回按钮才能回到项目而不是跳到列表。
    ///
    /// @author kongweiguang
    fn open_host_container_inspector(
        &mut self,
        target: ContainerTarget,
        tab: Option<HostContainerTab>,
        project_origin: Option<ComposeProjectTarget>,
        cx: &mut Context<Self>,
    ) {
        let ticket = self.container_action_ticket(Some(target.clone()));
        self.leave_container_page_scroll();
        match project_origin {
            Some(project) => {
                self.container_host
                    .select_container_in_project(target.clone(), project, tab);
            }
            None => {
                self.container_host
                    .select_container_for_inspection(target.clone(), tab);
            }
        }
        let kind = match self.container_host.container_tab() {
            HostContainerTab::Details => ContainerReadKind::Inspect,
            HostContainerTab::Stats => ContainerReadKind::Stats,
        };
        self.request_container_read(target, kind, &ticket, cx);
        cx.notify();
    }
}

/// 主机容器列表的分组方式与 main 版工具栏选项保持同一语义。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::features::operations) enum ContainerHostGroupMode {
    #[default]
    Compose,
    Status,
    Flat,
}

/// Compose 页面只保存导航位置；YAML 预览通过共享文件服务读取，编辑入口另开中央工作区。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::features::operations) enum ComposeProjectTab {
    #[default]
    Overview,
    Containers,
    Yaml,
}

/// 容器详情 tab 仅切换已支持的 inspect 与 stats 请求。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::features::operations) enum HostContainerTab {
    #[default]
    Details,
    Stats,
}

/// 宿主页的目标与当前 tab 同生共死，不能出现无目标的详情页或跨容器残留 tab。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::features::operations) enum ContainerHostPage {
    #[default]
    List,
    ComposeProject {
        target: ComposeProjectTarget,
    },
    Container {
        target: ContainerTarget,
    },
}

/// 导航以宿主和 runtime 为作用域；同宿主不同 pane 保留页面，请求身份另由 action ticket 校验。
#[derive(Clone, Debug, PartialEq, Eq)]
struct ContainerHostScope {
    host_id: Option<HostId>,
    runtime: ContainerRuntime,
}

/// 容器宿主页的纯展示状态；请求、结果与权限门禁继续由 OperationsView/Root 所有。
#[derive(Clone, Debug)]
pub(in crate::features::operations) struct ContainerHostPresentation {
    scope: Option<ContainerHostScope>,
    include_stopped: bool,
    group_mode: ContainerHostGroupMode,
    page: ContainerHostPage,
    project_tab: ComposeProjectTab,
    container_tab: HostContainerTab,
    /// 容器详情从 Compose 项目容器页签进入时保留来源，返回按钮回到该项目的容器页签。
    container_origin: Option<ComposeProjectTarget>,
    selected_project: Option<ComposeProjectTarget>,
    selected_container: Option<ContainerTarget>,
    expanded_projects: Vec<ComposeProjectTarget>,
}

impl Default for ContainerHostPresentation {
    /// 初始展示与主版一致：Compose 分组、纳入停止容器，且停留在宿主列表。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            scope: None,
            include_stopped: true,
            group_mode: ContainerHostGroupMode::Compose,
            page: ContainerHostPage::List,
            project_tab: ComposeProjectTab::Overview,
            container_tab: HostContainerTab::Details,
            container_origin: None,
            selected_project: None,
            selected_container: None,
            expanded_projects: Vec::new(),
        }
    }
}

impl ContainerHostPresentation {
    /// 仅在实际宿主/runtime 变化时清理导航；main 会保留搜索、分组和停止容器筛选。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn reset_for_target(
        &mut self,
        host_id: Option<&HostId>,
        runtime: ContainerRuntime,
    ) -> bool {
        let next_scope = Some(ContainerHostScope {
            host_id: host_id.cloned(),
            runtime,
        });
        if self.scope == next_scope {
            return false;
        }
        self.page = ContainerHostPage::List;
        self.expanded_projects.clear();
        self.project_tab = ComposeProjectTab::Overview;
        self.container_tab = HostContainerTab::Details;
        self.container_origin = None;
        self.selected_project = None;
        self.selected_container = None;
        self.scope = next_scope;
        true
    }

    /// 返回主机列表当前显示的停止容器开关值，供 typed list query 使用。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn include_stopped(&self) -> bool {
        self.include_stopped
    }

    /// 更新停止容器筛选并指示调用方是否应重读列表。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn set_include_stopped(
        &mut self,
        include_stopped: bool,
    ) -> bool {
        let changed = self.include_stopped != include_stopped;
        self.include_stopped = include_stopped;
        changed
    }

    /// 返回分组方式，保证下拉状态与列表投影共享同一来源。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn group_mode(&self) -> ContainerHostGroupMode {
        self.group_mode
    }

    /// 更新分组方式并指示调用方是否需要重建列表投影。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn set_group_mode(
        &mut self,
        group_mode: ContainerHostGroupMode,
    ) -> bool {
        let changed = self.group_mode != group_mode;
        self.group_mode = group_mode;
        changed
    }

    /// 暴露当前页面的不可变快照，渲染分支不复制或猜测详情目标。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn page(&self) -> &ContainerHostPage {
        &self.page
    }

    /// 返回 Compose 项目是否展开，按完整执行 target 匹配而非展示名称。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn is_project_expanded(
        &self,
        target: &ComposeProjectTarget,
    ) -> bool {
        self.expanded_projects
            .iter()
            .any(|expanded| expanded == target)
    }

    /// 选中项目按完整 target 保留，返回列表时仍能表现原选择。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn is_project_selected(
        &self,
        target: &ComposeProjectTarget,
    ) -> bool {
        self.selected_project.as_ref() == Some(target)
    }

    /// 选中容器按完整 target 保留，列表重排后仍能标出同一对象。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn is_container_selected(
        &self,
        target: &ContainerTarget,
    ) -> bool {
        self.selected_container.as_ref() == Some(target)
    }

    /// 切换 Compose 项目折叠状态，完整 target 防止同名项目相互影响。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn toggle_project_expanded(
        &mut self,
        target: ComposeProjectTarget,
    ) {
        if let Some(index) = self
            .expanded_projects
            .iter()
            .position(|expanded| expanded == &target)
        {
            self.expanded_projects.remove(index);
        } else {
            self.expanded_projects.push(target);
        }
    }

    /// 返回宿主列表时清空容器来源；列表自身的筛选和展开状态保留。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn navigate_to_list(&mut self) {
        self.page = ContainerHostPage::List;
        self.container_origin = None;
    }

    /// 打开项目详情时回到 Overview，与主版每次从列表选择项目的行为一致。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn select_project(&mut self, target: ComposeProjectTarget) {
        self.selected_project = Some(target.clone());
        self.selected_container = None;
        self.container_origin = None;
        self.project_tab = ComposeProjectTab::Overview;
        self.page = ContainerHostPage::ComposeProject { target };
    }

    /// 行菜单操作只更新选择，保留当前列表或项目页面；项目页内不丢项目选择。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn select_container_row(
        &mut self,
        target: ContainerTarget,
    ) {
        let on_project_page = matches!(self.page, ContainerHostPage::ComposeProject { .. });
        self.selected_container = Some(target);
        if !on_project_page {
            self.selected_project = None;
        }
    }

    /// 普通列表导航沿用当前 tab；Compose 子页和显式入口可传入 tab 覆盖默认状态。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn select_container_for_inspection(
        &mut self,
        target: ContainerTarget,
        tab: Option<HostContainerTab>,
    ) {
        self.select_container_with_origin(target, None, tab);
    }

    /// 从项目容器页签进入时记录来源项目，返回时恢复该项目容器页签与选中对象。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn select_container_in_project(
        &mut self,
        target: ContainerTarget,
        project: ComposeProjectTarget,
        tab: Option<HostContainerTab>,
    ) {
        self.select_container_with_origin(target, Some(project), tab);
    }

    /// 容器页的来源与 tab 一次写入，避免页面、项目选择和返回目标分属不同步更新。
    ///
    /// @author kongweiguang
    fn select_container_with_origin(
        &mut self,
        target: ContainerTarget,
        origin: Option<ComposeProjectTarget>,
        tab: Option<HostContainerTab>,
    ) {
        self.selected_container = Some(target.clone());
        self.selected_project = origin.clone();
        self.container_origin = origin;
        if self.container_origin.is_some() {
            self.project_tab = ComposeProjectTab::Containers;
        }
        if let Some(tab) = tab {
            self.container_tab = tab;
        }
        self.page = ContainerHostPage::Container { target };
    }

    /// 返回来源项目并保留项目容器页签与容器行选中；来源由调用方确认后才使用。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn return_to_project(
        &mut self,
        project: ComposeProjectTarget,
    ) {
        self.selected_project = Some(project.clone());
        self.container_origin = None;
        self.project_tab = ComposeProjectTab::Containers;
        self.page = ContainerHostPage::ComposeProject { target: project };
    }

    /// 返回当前容器详情记录的来源项目；列表和其它入口进入的详情为 None。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn container_origin(
        &self,
    ) -> Option<&ComposeProjectTarget> {
        self.container_origin.as_ref()
    }

    /// 更新项目详情 tab，仅对当前项目页有效。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn set_project_tab(&mut self, next_tab: ComposeProjectTab) {
        self.project_tab = next_tab;
    }

    /// 更新容器详情 tab，仅对当前 inspector 有效。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn set_container_tab(
        &mut self,
        next_tab: HostContainerTab,
    ) {
        self.container_tab = next_tab;
    }

    /// 返回当前 Compose 页签，列表与其它详情页不改写该值。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn project_tab(&self) -> ComposeProjectTab {
        self.project_tab
    }

    /// 返回当前容器页签，容器目标变化时仍由 target-specific 读取状态隔离内容。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn container_tab(&self) -> HostContainerTab {
        self.container_tab
    }
}

/// 根据 Root 提供的类型化上下文绘制对应面板，绝不从主机选择状态推断容器目标。
///
/// @author kongweiguang
pub(super) fn render(
    view: &mut OperationsView,
    context: &ContainerPanelContext,
    theme: ThemeTokens,
    cx: &mut Context<OperationsView>,
) -> AnyElement {
    match context {
        ContainerPanelContext::Unavailable { reason } => render_unavailable(*reason, theme),
        ContainerPanelContext::Host { .. } | ContainerPanelContext::WorkspaceHost { .. } => {
            render_host(view, theme, cx)
        }
        ContainerPanelContext::ContainerInfo {
            target,
            terminal_running,
            ..
        } => render_container_info(view, target, *terminal_running, theme, cx),
    }
}

/// 不可用状态展示 Root 给出的具体原因，不尝试读取本机或切换到其它容器。
///
/// @author kongweiguang
fn render_unavailable(reason: ContainerPanelUnavailableReason, theme: ThemeTokens) -> AnyElement {
    let (zh, en) = reason.label();
    super::section_page(
        crate::locale::text("容器", "Containers"),
        div()
            .w_full()
            .min_h(px(90.))
            .flex()
            .items_center()
            .justify_center()
            .p_3()
            .text_size(theme.text_sm)
            .text_color(theme.muted)
            .child(crate::locale::text(zh, en)),
        theme,
        true,
        true,
    )
    .into_any_element()
}

/// 宿主模式只列当前 Root 绑定的 host/runtime；常用读取保持直达，写操作集中在菜单。
/// 删除确认展开行 ticket 冻结的完整目标，窄栏也能核对，且不从当前选择推导宿主。
///
/// @author kongweiguang
fn render_host(
    view: &mut OperationsView,
    theme: ThemeTokens,
    cx: &mut Context<OperationsView>,
) -> AnyElement {
    match view.container_host.page().clone() {
        ContainerHostPage::List => render_host_list(view, theme, cx),
        ContainerHostPage::ComposeProject { target } => {
            render_host_project(view, target, theme, cx)
        }
        ContainerHostPage::Container { target } => render_host_container(view, target, theme, cx),
    }
}

/// 宿主列表只展示当前 binding/runtime 的数据；页头和筛选共用工具面板密度与对齐边界。
/// 同目标刷新不插入临时占位，长主机名与摘要可通过提示查看完整内容。
///
/// @author kongweiguang
fn render_host_list(
    view: &mut OperationsView,
    theme: ThemeTokens,
    cx: &mut Context<OperationsView>,
) -> AnyElement {
    let ticket = view.container_action_ticket(None);
    let busy = view.busy
        || view.container_write_scope_blocked()
        || view.containers.phase() != super::super::state::SectionPhase::Ready;
    let runtime = view.runtime;
    let rows = view.containers.data().cloned().unwrap_or_default();
    let status_filter = if view.container_host.include_stopped() {
        ContainerStatusFilter::All
    } else {
        ContainerStatusFilter::Running
    };
    let filtered = filter_containers(&rows, &view.container_search, status_filter);
    let retained =
        if view.container_search.trim().is_empty() && view.container_host.include_stopped() {
            view.compose_projects.as_slice()
        } else {
            &[]
        };
    let groups =
        host_container_groups(filtered.clone(), retained, view.container_host.group_mode());
    let inputs = view
        .inputs
        .as_ref()
        .expect("inputs initialized before render");
    let loading = matches!(
        view.containers.phase(),
        super::super::state::SectionPhase::NotLoaded | super::super::state::SectionPhase::Loading
    );
    let summary = view.containers.data().map_or_else(String::new, |_| {
        container_summary(&rows, &groups, false, false)
    });
    let empty_result =
        view.containers.data().is_some() && filtered.is_empty() && retained.is_empty();
    let show_list_feedback = !loading
        && (view.containers.error().is_some()
            || view.containers.phase() == super::super::state::SectionPhase::Cancelled
            || (view.containers.phase() == super::super::state::SectionPhase::Ready
                && empty_result));
    let runtime_entity = cx.entity().clone();
    let runtime_ticket = ticket.clone();
    let runtime_menu_theme = theme;
    let runtime_label = match runtime {
        ContainerRuntime::Docker => "Docker",
        ContainerRuntime::Podman => "Podman",
    };
    let runtime_select = Button::new("container-panel-runtime-select")
        .debug_selector(|| "rr-containers-runtime".to_owned())
        .compact()
        .dropdown_caret(true)
        .w_full()
        .h(theme.metrics.control_height)
        .accessibility_label(runtime_label)
        .child(container_selector_label(runtime_label, theme))
        .dropdown_menu(move |mut menu, _, _| {
            menu = crate::ui_popup::dropdown_menu(menu, runtime_menu_theme);
            for (option, label) in [
                (ContainerRuntime::Docker, "Docker"),
                (ContainerRuntime::Podman, "Podman"),
            ] {
                let entity = runtime_entity.clone();
                let ticket = runtime_ticket.clone();
                let selected = option == runtime;
                menu = menu.item(PopupMenuItem::new(label).checked(selected).on_click(
                    move |_, _, app| {
                        entity.update(app, |view, cx| {
                            view.request_container_panel_refresh(option, &ticket, cx);
                        });
                    },
                ));
            }
            menu
        })
        .offset(px(6.));
    let group_mode = view.container_host.group_mode();
    let group_label = container_group_mode_label(group_mode);
    let group_ticket = ticket.clone();
    let group_entity = cx.entity().clone();
    let group_menu_theme = theme;
    let group_select = Button::new("container-panel-group-mode")
        .debug_selector(|| "rr-containers-group".to_owned())
        .compact()
        .dropdown_caret(true)
        .w_full()
        .h(theme.metrics.control_height)
        .accessibility_label(group_label.clone())
        .child(container_selector_label(group_label, theme))
        .dropdown_menu(move |mut menu, _, _| {
            menu = crate::ui_popup::dropdown_menu(menu, group_menu_theme);
            for mode in [
                ContainerHostGroupMode::Compose,
                ContainerHostGroupMode::Status,
                ContainerHostGroupMode::Flat,
            ] {
                let entity = group_entity.clone();
                let ticket = group_ticket.clone();
                let selected = mode == group_mode;
                menu = menu.item(
                    PopupMenuItem::new(container_group_mode_label(mode))
                        .checked(selected)
                        .on_click(move |_, _, app| {
                            entity.update(app, |view, cx| {
                                if view.container_action_ticket_is_current(&ticket, None) {
                                    view.container_host.set_group_mode(mode);
                                    cx.notify();
                                }
                            });
                        }),
                );
            }
            menu
        })
        .offset(px(6.));
    let include_stopped = view.container_host.include_stopped();
    let host_name = view.host_target.as_ref().map_or_else(
        || crate::locale::text("本机终端", "Local terminal"),
        |host_id| {
            view.host_display_names
                .get(host_id)
                .cloned()
                .unwrap_or_else(|| host_id.as_str().into())
        },
    );
    let host_tooltip = host_name.clone();
    let summary_tooltip = summary.clone();
    let refresh_ticket = ticket.clone();
    let refresh_button = icon_button(
        "container-panel-refresh",
        gpui_kit::assets::IconName::RefreshCw,
        crate::locale::text("刷新容器列表", "Refresh containers"),
    )
    .w(theme.metrics.control_height)
    .h(theme.metrics.control_height)
    .debug_selector(|| "rr-containers-refresh".to_owned())
    .loading(loading)
    .loading_icon(gpui_kit::assets::IconName::RefreshCw)
    .disabled(loading || view.busy)
    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
        view.request_container_panel_refresh(runtime, &refresh_ticket, cx);
    }));
    let mut body = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            crate::ui_surface::panel_header(theme)
                .id("container-panel-header")
                .w_full()
                .flex_col()
                .items_stretch()
                .gap_2()
                .px_0()
                .pt_0()
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_start()
                        .justify_between()
                        .gap_2()
                        .child(
                            div().min_w_0().flex_1().flex().flex_col().gap_1().child(
                                div()
                                    .id(format!(
                                        "container-panel-host-name-{}",
                                        cx.entity_id().as_u64()
                                    ))
                                    .truncate()
                                    .text_size(theme.text_sm)
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .aria_label(host_name.clone())
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(host_tooltip.clone()).build(window, cx)
                                    })
                                    .child(host_name),
                            ),
                        )
                        .child(refresh_button),
                )
                .child(
                    div()
                        .id(format!(
                            "container-panel-summary-{}",
                            cx.entity_id().as_u64()
                        ))
                        .w_full()
                        .min_w_0()
                        .min_h_4()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .truncate()
                        .when(!summary_tooltip.is_empty(), |row| {
                            row.tooltip(move |window, cx| {
                                Tooltip::new(summary_tooltip.clone()).build(window, cx)
                            })
                        })
                        .child(summary),
                ),
        )
        .child(
            div()
                .w_full()
                .flex_col()
                .gap_2()
                .child(
                    gpui_kit::Styled::h(
                        Input::new(&inputs.container_search)
                            .prefix(gpui_kit::assets::IconName::Search)
                            .w_full(),
                        theme.metrics.control_height,
                    )
                    .text_size(theme.text_sm)
                    .line_height(relative(1.5)),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .gap_2()
                        .child(div().flex_1().child(runtime_select))
                        .child(div().flex_1().child(group_select)),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .h(theme.metrics.control_height)
                        .px_3()
                        .rounded_xl()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.sidebar)
                        .child(crate::locale::text("包含停止容器", "Include stopped"))
                        .child(
                            Switch::new("container-include-stopped")
                                .checked(include_stopped)
                                .accessibility_label(crate::locale::text(
                                    "包含停止容器",
                                    "Include stopped containers",
                                ))
                                .disabled(busy)
                                .on_change(cx.listener(|view, enabled: &bool, _, cx| {
                                    view.set_host_container_include_stopped(*enabled, cx);
                                })),
                        ),
                ),
        )
        .children(view.compact_operation_feedback(theme))
        .children(container_success_notice(view, theme))
        .when(show_list_feedback, |body| {
            if view.containers.phase() == super::super::state::SectionPhase::Ready
                && view.containers.error().is_none()
                && empty_result
            {
                let message = if view.container_search.trim().is_empty() && include_stopped {
                    crate::locale::text("暂无容器", "No containers")
                } else {
                    crate::locale::text(
                        "没有符合当前筛选的容器",
                        "No containers match the current filters",
                    )
                };
                body.child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(message),
                )
            } else {
                body.child(super::section_feedback(
                    &view.containers,
                    empty_result,
                    crate::locale::text(
                        "尚未读取容器列表。",
                        "Container list has not been loaded.",
                    ),
                    theme,
                ))
            }
        });

    let mut container_list = div().w_full().min_w_0().flex().flex_col().gap_1p5();
    if group_mode == ContainerHostGroupMode::Compose {
        let project_groups = groups.iter().filter(|group| group.compose_group).count();
        if project_groups > 0 {
            container_list = container_list.child(container_section_heading(
                crate::locale::text("Compose 应用", "Compose projects"),
                crate::locale::text(
                    format!("{project_groups} 项"),
                    format!("{project_groups} projects"),
                ),
                gpui_kit::assets::IconName::Layers,
                theme,
            ));
        }
    }

    let standalone_count = groups
        .iter()
        .filter(|group| !group.compose_group)
        .map(|group| group.containers.len())
        .sum::<usize>();
    let inspector_theme = theme.for_inspector();
    let mut standalone_heading_added = false;
    for group in groups {
        if group.compose_group {
            if let Some(target) = group.target.as_ref() {
                let group_key = compose_project_element_key(target);
                let expanded = view.container_host.is_project_expanded(target);
                let project_target = target.clone();
                let expand_target = project_target.clone();
                let yaml_target = project_target.clone();
                let expand_ticket = ticket.clone();
                let yaml_ticket = ticket.clone();
                let running = group
                    .containers
                    .iter()
                    .filter(|item| item.status == ContainerStatus::Running)
                    .count();
                let project_status_color = if group
                    .containers
                    .iter()
                    .any(|item| item.status == ContainerStatus::Dead)
                {
                    theme.error
                } else if running > 0 {
                    theme.success
                } else {
                    theme.muted
                };
                let project_selected = view.container_host.is_project_selected(target);
                let toggle_selector = format!("rr-compose-toggle-{group_key}");
                let select_button = Button::new(format!("container-project-{group_key}"))
                    .compact()
                    .ghost()
                    .tab_stop(true)
                    .debug_selector(move || toggle_selector)
                    .accessibility_label(crate::locale::text(
                        format!(
                            "{} Compose 项目 {}",
                            if expanded { "折叠" } else { "展开" },
                            target.project
                        ),
                        format!(
                            "{} Compose project {}",
                            if expanded { "Collapse" } else { "Expand" },
                            target.project
                        ),
                    ))
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(gpui_kit::component::Icon::new(if expanded {
                                gpui_kit::assets::IconName::ChevronDown
                            } else {
                                gpui_kit::assets::IconName::ChevronRight
                            }))
                            .child(div().size_2().rounded_full().bg(project_status_color))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .text_left()
                                    .child(target.project.clone()),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .px_2()
                                    .py(px(2.))
                                    .rounded_full()
                                    .bg(inspector_theme.secondary)
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(format!("{running}/{}", group.containers.len())),
                            ),
                    )
                    .min_w_0()
                    .flex_1()
                    .text_color(theme.foreground)
                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                        if view.container_action_ticket_is_current(&expand_ticket, None) {
                            view.container_host
                                .toggle_project_expanded(expand_target.clone());
                            cx.notify();
                        }
                    }));
                let yaml_selector = format!("rr-compose-yaml-{group_key}");
                let yaml_button = icon_button(
                    format!("container-project-yaml-{group_key}"),
                    gpui_kit::assets::IconName::FileCode,
                    crate::locale::text("打开 Compose YAML", "Open Compose YAML"),
                )
                .debug_selector(move || yaml_selector)
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    if !view.container_action_ticket_is_current(&yaml_ticket, None) {
                        return;
                    }
                    view.leave_container_page_scroll();
                    view.container_host.select_project(yaml_target.clone());
                    view.container_host.set_project_tab(ComposeProjectTab::Yaml);
                    super::compose_yaml::request_preview_for_selected_path(
                        view,
                        &yaml_target,
                        &yaml_ticket,
                        cx,
                    );
                    cx.notify();
                }));
                let project_row = div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px(px(10.))
                    .py_2()
                    .bg(if project_selected {
                        theme.surface_selected()
                    } else {
                        theme.panel.opacity(0.64)
                    })
                    .when(!project_selected, |row| {
                        row.hover(|style| style.bg(theme.surface_hover()))
                    })
                    .child(select_button)
                    .child(yaml_button)
                    .child(compose_actions_menu(
                        target,
                        &ticket,
                        busy || !view.compose_mutation_allowed(target),
                        view.container_mutations.compose_loading(target),
                        theme,
                        format!("container-project-more-{group_key}"),
                        cx,
                    ))
                    .children(container_cancel_button(
                        view,
                        view.container_mutations.compose_executing(target),
                        format!("container-project-cancel-{group_key}"),
                        cx,
                    ));
                let mut project_card = div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .rounded(theme.control_radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.panel.opacity(0.64))
                    .shadow_sm()
                    .child(project_row);
                if expanded {
                    let mut project_containers = div()
                        .w_full()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .border_t_1()
                        .border_color(theme.border)
                        .p(px(6.))
                        .bg(theme.subtle_background());
                    for (index, container) in group.containers.into_iter().enumerate() {
                        if index > 0 {
                            project_containers =
                                project_containers.child(div().w_full().h(px(1.)).bg(theme.border));
                        }
                        project_containers = project_containers.child(render_host_container_row(
                            view, container, true, None, busy, theme, cx,
                        ));
                    }
                    project_card = project_card.child(project_containers);
                }
                container_list = container_list.child(project_card);
            } else {
                container_list = container_list.child(container_group_heading(
                    group.project.as_deref().unwrap_or("Compose"),
                    &group.containers,
                    theme,
                ));
                for container in group.containers {
                    container_list = container_list.child(render_host_container_row(
                        view, container, true, None, busy, theme, cx,
                    ));
                }
            }
        } else if !group.containers.is_empty() {
            if group_mode != ContainerHostGroupMode::Compose {
                let title = group.project.as_deref().unwrap_or("全部容器");
                container_list =
                    container_list.child(container_group_heading(title, &group.containers, theme));
            } else if !standalone_heading_added && standalone_count > 0 {
                container_list = container_list.child(container_section_heading(
                    crate::locale::text("独立容器", "Standalone containers"),
                    crate::locale::text(
                        format!("{standalone_count} 个"),
                        format!("{standalone_count} containers"),
                    ),
                    gpui_kit::assets::IconName::Box,
                    theme,
                ));
                standalone_heading_added = true;
            }
            for container in group.containers {
                container_list = container_list.child(render_host_container_row(
                    view, container, false, None, busy, theme, cx,
                ));
            }
        }
    }

    body = body.child(container_list);
    super::section_page(
        crate::locale::text("宿主容器", "Host containers"),
        body,
        theme,
        true,
        true,
    )
    .into_any_element()
}

/// 列表可能因状态刷新重新排序，控件身份必须随完整项目而不是行序号移动。
///
/// @author kongweiguang
fn compose_project_element_key(target: &ComposeProjectTarget) -> String {
    format!(
        "{:?}",
        (
            target.host_id.as_ref().map(|host| host.as_str()),
            target.runtime,
            &target.project,
            &target.working_dir,
            &target.config_files,
        )
    )
}

/// 按 main 的 Compose、状态与平铺模式投影分组，完整 Compose target 才能进入项目详情。
///
/// @author kongweiguang
fn host_container_groups(
    containers: Vec<kerminal_domain::operations::ContainerSummary>,
    retained_projects: &[kerminal_domain::operations::ComposeProjectTarget],
    mode: ContainerHostGroupMode,
) -> Vec<ContainerProjectGroup> {
    match mode {
        ContainerHostGroupMode::Compose => {
            let mut groups = group_container_projects_with_retained(containers, retained_projects);
            groups.sort_by(|left, right| {
                let left_standalone = !left.compose_group;
                let right_standalone = !right.compose_group;
                left_standalone
                    .cmp(&right_standalone)
                    .then_with(|| {
                        let left_running = left
                            .containers
                            .iter()
                            .filter(|item| item.status == ContainerStatus::Running)
                            .count();
                        let right_running = right
                            .containers
                            .iter()
                            .filter(|item| item.status == ContainerStatus::Running)
                            .count();
                        right_running.cmp(&left_running)
                    })
                    .then_with(|| left.project.cmp(&right.project))
            });
            groups
        }
        ContainerHostGroupMode::Status => {
            let mut groups = Vec::new();
            for status in [
                ContainerStatus::Running,
                ContainerStatus::Exited,
                ContainerStatus::Paused,
                ContainerStatus::Restarting,
                ContainerStatus::Created,
                ContainerStatus::Dead,
                ContainerStatus::Unknown,
            ] {
                let group_containers = containers
                    .iter()
                    .filter(|item| item.status == status)
                    .cloned()
                    .collect::<Vec<_>>();
                if !group_containers.is_empty() {
                    groups.push(ContainerProjectGroup {
                        project: Some(container_status_group_label(status).to_string()),
                        compose_group: false,
                        target: None,
                        containers: group_containers,
                    });
                }
            }
            groups
        }
        ContainerHostGroupMode::Flat => {
            if containers.is_empty() {
                Vec::new()
            } else {
                vec![ContainerProjectGroup {
                    project: Some(crate::locale::text("全部容器", "All containers").to_string()),
                    compose_group: false,
                    target: None,
                    containers,
                }]
            }
        }
    }
}

/// 以领域状态命名状态分组，避免根据 Docker 返回的可变 status 文案建组。
///
/// @author kongweiguang
fn container_status_group_label(status: ContainerStatus) -> gpui_kit::SharedString {
    match status {
        ContainerStatus::Running => crate::locale::text("运行中", "Running"),
        ContainerStatus::Exited => crate::locale::text("已停止", "Stopped"),
        ContainerStatus::Paused => crate::locale::text("已暂停", "Paused"),
        ContainerStatus::Restarting => crate::locale::text("重启中", "Restarting"),
        ContainerStatus::Created => crate::locale::text("已创建", "Created"),
        ContainerStatus::Dead => crate::locale::text("异常", "Dead"),
        ContainerStatus::Unknown => crate::locale::text("未知", "Unknown"),
    }
}

/// 分组菜单与当前界面语言同步，避免切换语言后只改变部分选项。
///
/// @author kongweiguang
fn container_group_mode_label(mode: ContainerHostGroupMode) -> SharedString {
    match mode {
        ContainerHostGroupMode::Compose => "Compose".into(),
        ContainerHostGroupMode::Status => crate::locale::text("状态", "Status"),
        ContainerHostGroupMode::Flat => crate::locale::text("平铺", "Flat"),
    }
}

/// 选择器显式继承工具栏文字尺度，避免 Kit 紧凑按钮覆盖共享密度和界面缩放。
///
/// @author kongweiguang
fn container_selector_label(label: impl Into<SharedString>, theme: ThemeTokens) -> AnyElement {
    let label: SharedString = label.into();
    div()
        .min_w_0()
        .whitespace_nowrap()
        .text_ellipsis()
        .text_size(theme.text_sm)
        .line_height(relative(1.5))
        .font_weight(gpui_kit::FontWeight::NORMAL)
        .child(label)
        .into_any_element()
}

/// 顶部摘要只从当前返回的容器快照计算，不用失败读取伪造零值。
///
/// @author kongweiguang
fn container_summary(
    rows: &[kerminal_domain::operations::ContainerSummary],
    groups: &[ContainerProjectGroup],
    loading: bool,
    failed: bool,
) -> String {
    if loading {
        return crate::locale::text("正在检查容器", "Checking containers").to_string();
    }
    if failed {
        return crate::locale::text("容器读取失败", "Container read failed").to_string();
    }
    let running = rows
        .iter()
        .filter(|item| item.status == ContainerStatus::Running)
        .count();
    let stopped = rows.len().saturating_sub(running);
    let projects = groups.iter().filter(|group| group.compose_group).count();
    let standalone = groups
        .iter()
        .filter(|group| !group.compose_group)
        .map(|group| group.containers.len())
        .sum::<usize>();
    let errors = rows
        .iter()
        .filter(|item| item.status == ContainerStatus::Dead)
        .count();
    let mut parts = vec![
        format!("{} {}", running, crate::locale::text("运行", "running")),
        format!("{} {}", stopped, crate::locale::text("停止", "stopped")),
        format!("{} Compose", projects),
        format!(
            "{} {}",
            standalone,
            crate::locale::text("独立", "standalone")
        ),
    ];
    if errors > 0 {
        parts.push(format!(
            "{} {}",
            errors,
            crate::locale::text("异常", "errors")
        ));
    }
    parts.join(" · ")
}

/// 固定宿主列表的 Compose/独立容器分区复用主版图标、计数和稳定对齐列。
///
/// @author kongweiguang
fn container_section_heading(
    title: gpui_kit::SharedString,
    count: gpui_kit::SharedString,
    icon: gpui_kit::assets::IconName,
    theme: ThemeTokens,
) -> AnyElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .h(px(32.))
        .px_2()
        .rounded_xl()
        .bg(theme.materials.chrome)
        .child(
            div()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(icon)
                .child(div().min_w_0().truncate().child(title)),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(count),
        )
        .into_any_element()
}

/// 分组标题沿用列表的稳定扫描层级，只显示当前分组内的运行数和总数。
///
/// @author kongweiguang
fn container_group_heading(
    title: &str,
    containers: &[kerminal_domain::operations::ContainerSummary],
    theme: ThemeTokens,
) -> AnyElement {
    let running = containers
        .iter()
        .filter(|item| item.status == ContainerStatus::Running)
        .count();
    div()
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .h(px(32.))
        .px_2()
        .rounded_xl()
        .bg(theme.materials.chrome)
        .text_size(theme.text_xs)
        .text_color(theme.muted)
        .child(
            div()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .child(gpui_kit::assets::IconName::Box)
                .child(div().min_w_0().truncate().child(title.to_owned())),
        )
        .child(crate::locale::text(
            format!("{running}/{} 运行", containers.len()),
            format!("{running}/{} running", containers.len()),
        ))
        .into_any_element()
}

/// 容器行只显示主操作，日志、固定和生命周期命令留在更多菜单，详情和监控由 inspector 页签承载。
/// 行身份使用完整容器 ID，避免短 ID 冲突或刷新重排时的测试目标漂移。
///
/// @author kongweiguang
fn render_host_container_row(
    view: &mut OperationsView,
    container: kerminal_domain::operations::ContainerSummary,
    compact: bool,
    project_origin: Option<&ComposeProjectTarget>,
    busy: bool,
    theme: ThemeTokens,
    cx: &mut Context<OperationsView>,
) -> AnyElement {
    let target = container.target.clone();
    let ticket = view.container_action_ticket(Some(target.clone()));
    let selected = view.container_host.is_container_selected(&target);
    let pending = view.container_mutations.container_loading(&target);
    let row_id = target.container_id.clone();
    let project_origin = project_origin.cloned();
    let project_detail = project_origin.is_some();
    let display_name = if project_detail {
        &container.name
    } else if compact {
        container
            .compose
            .as_ref()
            .and_then(|compose| compose.service.as_deref())
            .filter(|service| !service.trim().is_empty())
            .unwrap_or(&container.name)
    } else {
        &container.name
    };
    let secondary_text = if project_detail {
        Some(format!("{} · {}", container.name, container.image))
    } else if !container.ports.is_empty() {
        Some(container.ports.join(", "))
    } else {
        None
    };
    let status_color = match container.status {
        ContainerStatus::Running => theme.success,
        ContainerStatus::Dead => theme.error,
        ContainerStatus::Paused | ContainerStatus::Restarting => theme.warning,
        ContainerStatus::Exited | ContainerStatus::Created | ContainerStatus::Unknown => {
            theme.muted
        }
    };
    let status_label = container_status_group_label(container.status);
    let info_target = target.clone();
    let info_ticket = ticket.clone();
    let info_status = container.status;
    let info_project_detail = project_detail;
    let info_origin = project_origin.clone();
    let row_selector = format!("rr-container-row-{}", target.container_id);
    let accessibility_label = if project_detail {
        crate::locale::text(
            format!("容器 {}，{}，单击查看详情", container.name, status_label),
            format!(
                "Container {}; {}; click for details",
                container.name, status_label
            ),
        )
    } else if container.status == ContainerStatus::Running {
        crate::locale::text(
            format!(
                "容器 {}，{}，单击或按空格查看详情，按 Enter 进入终端",
                container.name, status_label
            ),
            format!(
                "Container {}; {}; click or press Space for details; press Enter to open a terminal",
                container.name, status_label
            ),
        )
    } else {
        crate::locale::text(
            format!("容器 {}，{}，单击查看详情", container.name, status_label),
            format!(
                "Container {}; {}; click for details",
                container.name, status_label
            ),
        )
    };
    // 名称、状态和副标题同属一个语义按钮；空格执行详情，Enter 保留既有终端快捷入口。
    // 单击立即进入详情，不再承诺双击进入终端，避免首击已切页后第二击落到新页面。
    let info = Button::new(format!("container-row-{row_id}"))
        .compact()
        .ghost()
        .debug_selector(move || row_selector)
        .accessibility_label(accessibility_label)
        .min_w_0()
        .flex_1()
        .h_auto()
        .min_h(px(40.))
        .p_0()
        .justify_start()
        .on_click(cx.listener(move |view, event: &ClickEvent, _, cx| {
            if !view.container_action_ticket_is_current(&info_ticket, Some(&info_target)) {
                return;
            }
            match container_row_activation(info_project_detail, event, info_status) {
                ContainerRowActivation::Inspect => view.open_host_container_inspector(
                    info_target.clone(),
                    if info_project_detail {
                        Some(HostContainerTab::Details)
                    } else {
                        None
                    },
                    info_origin.clone(),
                    cx,
                ),
                ContainerRowActivation::Enter => {
                    view.open_host_container_inspector(info_target.clone(), None, None, cx);
                    view.request_container_exec(info_target.clone(), &info_ticket, cx);
                }
            }
        }))
        .child(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .items_start()
                .gap_1()
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .size_2()
                                .flex_shrink_0()
                                .rounded_full()
                                .bg(status_color),
                        )
                        .child(div().min_w_0().truncate().child(display_name.to_owned()))
                        .child(status_pill(
                            status_label.to_string(),
                            container.status,
                            theme,
                        )),
                )
                .when_some(secondary_text, |this, secondary| {
                    this.child(
                        div()
                            .w_full()
                            .min_w_0()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .truncate()
                            .child(secondary),
                    )
                }),
        );
    let mut row = div()
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .border_1()
        .border_color(if selected {
            theme.accent.opacity(0.4)
        } else if compact {
            theme.border.opacity(0.0)
        } else {
            theme.border
        })
        .when(selected, |row| row.bg(theme.selected))
        .when(!compact && !selected, |row| {
            row.bg(theme.tool_panel_surface.opacity(0.72))
        })
        .when(!selected, |row| row.hover(|style| style.bg(theme.hover)))
        .child(info);
    if compact {
        row = row.px_2().py(px(6.)).rounded_xl();
    } else {
        row = row.px(px(10.)).py_2().rounded_xl();
    }

    let mut actions = div().flex().items_center().gap_1();
    if project_detail {
        let logs_target = target.clone();
        let logs_ticket = ticket.clone();
        actions = actions.child(
            icon_button(
                format!("container-logs-{row_id}"),
                gpui_kit::assets::IconName::ScrollText,
                crate::locale::text(
                    format!("查看容器 {} 日志", container.name),
                    format!("View logs for {}", container.name),
                ),
            )
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                view.container_host
                    .select_container_row(logs_target.clone());
                view.request_container_logs(logs_target.clone(), &logs_ticket, cx);
                cx.notify();
            })),
        );
        let terminal_target = target.clone();
        let terminal_ticket = ticket.clone();
        actions = actions.child(
            icon_button(
                format!("container-enter-{row_id}"),
                gpui_kit::assets::IconName::Terminal,
                crate::locale::text(
                    format!("进入容器 {}", container.name),
                    format!("Enter container {}", container.name),
                ),
            )
            .disabled(container.status != ContainerStatus::Running || busy)
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                view.container_host
                    .select_container_row(terminal_target.clone());
                view.request_container_exec(terminal_target.clone(), &terminal_ticket, cx);
            })),
        );
    } else if container.status == ContainerStatus::Running {
        let terminal_target = target.clone();
        let terminal_ticket = ticket.clone();
        actions = actions.child(
            icon_button(
                format!("container-enter-{row_id}"),
                gpui_kit::assets::IconName::Terminal,
                crate::locale::text(
                    format!("进入容器 {}", container.name),
                    format!("Open terminal for {}", container.name),
                ),
            )
            .disabled(busy)
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                view.container_host
                    .select_container_row(terminal_target.clone());
                view.request_container_exec(terminal_target.clone(), &terminal_ticket, cx);
            })),
        );
    } else {
        let can_start = can_run_host_container_action(container.status, ContainerAction::Start);
        let start_target = target.clone();
        let start_ticket = ticket.clone();
        actions = actions.child(
            icon_button(
                format!("container-start-{row_id}"),
                gpui_kit::assets::IconName::Play,
                crate::locale::text(
                    format!("启动容器 {}", container.name),
                    format!("Start container {}", container.name),
                ),
            )
            .disabled(busy || !can_start)
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                view.container_host
                    .select_container_row(start_target.clone());
                view.request_container_mutation(
                    ContainerMutationRequest {
                        target: start_target.clone(),
                        action: ContainerAction::Start,
                        force: false,
                    },
                    &start_ticket,
                    cx,
                );
            })),
        );
    }
    if !project_detail {
        actions = actions.child(container_actions_menu(
            &container,
            &ticket,
            busy,
            pending,
            theme,
            format!("container-more-{row_id}"),
            cx,
        ));
        actions = actions.children(container_cancel_button(
            view,
            view.container_mutations.container_executing(&target),
            format!("container-cancel-{row_id}"),
            cx,
        ));
    }
    row = row.child(actions);
    row.into_any_element()
}

/// 根据当前状态复用 main 的生命周期可用条件，避免把强制删除或不支持状态增加到菜单。
///
/// @author kongweiguang
fn can_run_host_container_action(status: ContainerStatus, action: ContainerAction) -> bool {
    match action {
        ContainerAction::Start => !matches!(
            status,
            ContainerStatus::Running | ContainerStatus::Restarting
        ),
        ContainerAction::Stop => matches!(
            status,
            ContainerStatus::Running | ContainerStatus::Paused | ContainerStatus::Restarting
        ),
        ContainerAction::Restart => {
            matches!(status, ContainerStatus::Running | ContainerStatus::Paused)
        }
        ContainerAction::Remove => !matches!(
            status,
            ContainerStatus::Running | ContainerStatus::Restarting
        ),
    }
}

/// 容器行菜单按查看、辅助操作和生命周期分组；详情页不重复自身入口。
///
/// @author kongweiguang
fn container_actions_menu(
    container: &kerminal_domain::operations::ContainerSummary,
    ticket: &super::super::container_panel::ContainerPanelActionTicket,
    busy: bool,
    pending: bool,
    theme: ThemeTokens,
    id: String,
    cx: &mut Context<OperationsView>,
) -> impl IntoElement + 'static {
    let entity = cx.entity().clone();
    let show_details = id != "container-inspector-more";
    let target = container.target.clone();
    let debug_selector = format!("rr-container-more-{}", target.container_id);
    let container_name = container.name.clone();
    let container_snapshot = container.clone();
    let status = container.status;
    let menu_ticket = ticket.clone();
    icon_button(
        id,
        gpui_kit::assets::IconName::Ellipsis,
        crate::locale::text(
            format!("更多容器操作 {}", container.name),
            format!("More actions for {}", container.name),
        ),
    )
    .h_7()
    .w_7()
    .rounded_lg()
    .debug_selector(move || debug_selector)
    .loading(pending)
    .dropdown_menu(move |mut menu, _, _| {
        menu = crate::ui_popup::context_menu(menu, theme, Some(px(176.)));
        if show_details {
            menu = menu.item(
                PopupMenuItem::new(crate::locale::text("查看详情", "View details"))
                    .icon(gpui_kit::assets::IconName::Info)
                    .on_click({
                        let target = target.clone();
                        let ticket = menu_ticket.clone();
                        let entity = entity.clone();
                        move |_, _, app| {
                            entity.update(app, |view, cx| {
                                if view.container_action_ticket_is_current(&ticket, Some(&target)) {
                                    view.open_host_container_inspector(
                                        target.clone(),
                                        Some(HostContainerTab::Details),
                                        None,
                                        cx,
                                    );
                                }
                            });
                        }
                    }),
            );
        }
        menu = menu.item(
            PopupMenuItem::new(crate::locale::text("日志", "Logs"))
                .icon(gpui_kit::assets::IconName::ScrollText)
                .disabled(busy)
                .on_click({
                    let target = target.clone();
                    let ticket = menu_ticket.clone();
                    let entity = entity.clone();
                    move |_, _, app| {
                        entity.update(app, |view, cx| {
                            if !view.container_action_ticket_is_current(&ticket, Some(&target)) {
                                return;
                            }
                            view.container_host.select_container_row(target.clone());
                            view.request_container_logs(target.clone(), &ticket, cx);
                            cx.notify();
                        });
                    }
                }),
        );
        menu = menu.item(
            PopupMenuItem::new(crate::locale::text("固定", "Pin"))
                .icon(gpui_kit::assets::IconName::Pin)
                .disabled(busy)
                .on_click({
                    let container = container_snapshot.clone();
                    let ticket = menu_ticket.clone();
                    let entity = entity.clone();
                    move |_, _, app| {
                        entity.update(app, |view, cx| {
                            view.request_container_pin(container.clone(), &ticket, cx);
                        });
                    }
                }),
        );
        menu = menu.item(PopupMenuItem::separator());
        for action in [
            ContainerAction::Start,
            ContainerAction::Stop,
            ContainerAction::Restart,
            ContainerAction::Remove,
        ] {
            let target = target.clone();
            let name = container_name.clone();
            let ticket = menu_ticket.clone();
            let entity = entity.clone();
            let label = container_action_label(action);
            let icon = match action {
                ContainerAction::Start => gpui_kit::assets::IconName::Play,
                ContainerAction::Stop => gpui_kit::assets::IconName::Square,
                ContainerAction::Restart => gpui_kit::assets::IconName::RotateCw,
                ContainerAction::Remove => gpui_kit::assets::IconName::Trash,
            };
            let enabled = can_run_host_container_action(status, action);
            let is_destructive = matches!(action, ContainerAction::Stop | ContainerAction::Remove);
            let on_click = move |_: &ClickEvent,
                                 window: &mut gpui_kit::Window,
                                 app: &mut gpui_kit::App| {
                entity.update(app, |view, cx| {
                    if !view.container_action_ticket_is_current(&ticket, Some(&target)) {
                        return;
                    }
                    view.container_host.select_container_row(target.clone());
                    match action {
                        ContainerAction::Start => view.request_container_mutation(
                            ContainerMutationRequest {
                                target: target.clone(),
                                action,
                                force: false,
                            },
                            &ticket,
                            cx,
                        ),
                        ContainerAction::Stop | ContainerAction::Restart => view
                            .request_container_lifecycle_confirmation(
                                target.clone(),
                                name.clone(),
                                action,
                                window,
                                cx,
                            ),
                        ContainerAction::Remove => {
                            view.request_remove_container(target.clone(), name.clone(), window, cx)
                        }
                    }
                    cx.notify();
                });
            };
            let item = PopupMenuItem::new(label)
                .icon(icon)
                .when(is_destructive, |item| item.danger())
                .disabled(!enabled || busy)
                .on_click(on_click);
            menu = menu.item(item);
        }
        menu
    })
    .offset(px(6.))
}

/// 项目菜单与容器菜单使用同一尺寸、分组和危险色；查看详情不依赖写操作可用性。
///
/// @author kongweiguang
fn compose_actions_menu(
    target: &ComposeProjectTarget,
    ticket: &super::super::container_panel::ContainerPanelActionTicket,
    disabled: bool,
    loading: bool,
    theme: ThemeTokens,
    id: String,
    cx: &mut Context<OperationsView>,
) -> impl IntoElement + 'static {
    let entity = cx.entity().clone();
    let show_details = id != "container-project-inspector-more";
    let debug_selector = if id == "container-project-inspector-more" {
        "rr-compose-project-more".to_owned()
    } else {
        format!(
            "rr-compose-project-more-{}",
            compose_project_element_key(target)
        )
    };
    let target = target.clone();
    let ticket = ticket.clone();
    icon_button(
        id,
        gpui_kit::assets::IconName::Ellipsis,
        crate::locale::text(
            format!("更多 Compose 项目操作 {}", target.project),
            format!("More actions for Compose project {}", target.project),
        ),
    )
    .h_7()
    .w_7()
    .rounded_lg()
    .loading(loading)
    .debug_selector(move || debug_selector)
    .dropdown_menu(move |mut menu, _, _| {
        menu = crate::ui_popup::context_menu(menu, theme, Some(px(176.)));
        if show_details {
            menu = menu.item(
                PopupMenuItem::new(crate::locale::text("查看详情", "View details"))
                    .icon(gpui_kit::assets::IconName::Info)
                    .on_click({
                        let entity = entity.clone();
                        let target = target.clone();
                        let ticket = ticket.clone();
                        move |_, _, app| {
                            entity.update(app, |view, cx| {
                                if view.container_action_ticket_is_current(&ticket, None) {
                                    view.leave_container_page_scroll();
                                    view.container_host.select_project(target.clone());
                                    cx.notify();
                                }
                            });
                        }
                    }),
            );
            menu = menu.item(PopupMenuItem::separator());
        }
        for (action, zh, en, icon) in [
            (
                ComposeAction::Up,
                "启动",
                "Start",
                gpui_kit::assets::IconName::Play,
            ),
            (
                ComposeAction::Stop,
                "停止",
                "Stop",
                gpui_kit::assets::IconName::Square,
            ),
            (
                ComposeAction::Restart,
                "重启",
                "Restart",
                gpui_kit::assets::IconName::RotateCw,
            ),
            (
                ComposeAction::Down,
                "拆除",
                "Remove",
                gpui_kit::assets::IconName::Trash,
            ),
        ] {
            let entity = entity.clone();
            let target = target.clone();
            let ticket = ticket.clone();
            let label = crate::locale::text(zh, en);
            let is_destructive = matches!(action, ComposeAction::Stop | ComposeAction::Down);
            let item = PopupMenuItem::new(label)
                .icon(icon)
                .when(is_destructive, |item| item.danger())
                .disabled(disabled)
                .on_click(move |_, _, app| {
                    entity.update(app, |view, cx| {
                        view.request_compose_mutation(target.clone(), action, &ticket, cx);
                    });
                });
            menu = menu.item(item);
        }
        menu
    })
    .offset(px(6.))
}

/// 已确认回执使用轻量成功色，失败和待核对仍沿用现有错误提示。
///
/// @author kongweiguang
fn container_success_notice(view: &OperationsView, theme: ThemeTokens) -> Option<AnyElement> {
    view.container_success_notice.as_ref().map(|message| {
        div()
            .w_full()
            .min_w_0()
            .text_size(theme.text_xs)
            .text_color(theme.success)
            .child(message.clone())
            .into_any_element()
    })
}

/// 取消只发送协作信号，动画保留到服务回执；不能把取消点击当作远端已停止。
///
/// @author kongweiguang
fn container_cancel_button(
    view: &OperationsView,
    loading: bool,
    id: String,
    cx: &mut Context<OperationsView>,
) -> Option<AnyElement> {
    let cancellation = view.mutation_cancellation.as_ref().filter(|_| loading)?;
    Some(
        icon_button(
            id,
            gpui_kit::assets::IconName::X,
            crate::locale::text("取消容器操作", "Cancel container operation"),
        )
        .disabled(cancellation.is_cancelled())
        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| view.cancel_operation_mutation(cx)))
        .into_any_element(),
    )
}

/// 生命周期菜单在不同入口共用同一套动词。
///
/// @author kongweiguang
fn container_action_label(action: ContainerAction) -> gpui_kit::SharedString {
    match action {
        ContainerAction::Start => crate::locale::text("启动", "Start"),
        ContainerAction::Stop => crate::locale::text("停止", "Stop"),
        ContainerAction::Restart => crate::locale::text("重启", "Restart"),
        ContainerAction::Remove => crate::locale::text("删除", "Remove"),
    }
}

/// Compose 详情保留概览和容器页；YAML 在同一侧栏以工具按钮进入并按所选文件刷新。
///
/// @author kongweiguang
fn render_host_project(
    view: &mut OperationsView,
    target: ComposeProjectTarget,
    theme: ThemeTokens,
    cx: &mut Context<OperationsView>,
) -> AnyElement {
    let ticket = view.container_action_ticket(None);
    let all_rows = view.containers.data().cloned().unwrap_or_default();
    let project_groups =
        group_container_projects_with_retained(all_rows, std::slice::from_ref(&target));
    let project = project_groups
        .into_iter()
        .find(|group| group.target.as_ref() == Some(&target));
    let empty_containers = Vec::new();
    let project_containers = project
        .as_ref()
        .map_or(empty_containers.as_slice(), |group| {
            group.containers.as_slice()
        });
    let display_metadata = compose_project_display_metadata(&target, project_containers);
    let project_counts = project.as_ref().map(|group| {
        (
            group
                .containers
                .iter()
                .filter(|container| container.status == ContainerStatus::Running)
                .count(),
            group.containers.len(),
        )
    });
    let selected_tab = view.container_host.project_tab();
    let row_busy = view.busy
        || view.container_write_scope_blocked()
        || view.containers.phase() != super::super::state::SectionPhase::Ready;
    let primary_path = (selected_tab == ComposeProjectTab::Yaml)
        .then(|| view.compose_yaml.selected_path().map(str::to_owned))
        .flatten()
        .or_else(|| display_metadata.config_paths.first().cloned())
        .or_else(|| target.config_files.first().cloned())
        .or_else(|| (!target.working_dir.trim().is_empty()).then(|| target.working_dir.clone()))
        .unwrap_or_else(|| {
            crate::locale::text("未发现 Compose YAML 路径", "No Compose YAML path found")
                .to_string()
        });
    let mut tabs = div().w_full().min_w_0().flex().gap_1();
    for (tab, label, id) in [
        (
            ComposeProjectTab::Overview,
            crate::locale::text("概览", "Overview"),
            "overview",
        ),
        (
            ComposeProjectTab::Containers,
            crate::locale::text("容器", "Containers"),
            "containers",
        ),
    ] {
        let tab_ticket = ticket.clone();
        let button = Button::new(format!("container-project-tab-{id}"))
            .compact()
            .debug_selector(move || format!("rr-compose-tab-{id}"))
            .label(label)
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                if !view.container_action_ticket_is_current(&tab_ticket, None) {
                    return;
                }
                view.container_host.set_project_tab(tab);
                view.scroll_operations_to_top();
                view.clear_container_project_scroll();
                cx.notify();
            }));
        tabs = tabs.child(if selected_tab == tab {
            button.primary()
        } else {
            button.ghost()
        });
    }

    let back_ticket = ticket.clone();
    let back_target = target.clone();
    let refresh_ticket = ticket.clone();
    let refresh_target = target.clone();
    let yaml_ticket = ticket.clone();
    let yaml_target = target.clone();
    let yaml_selected = selected_tab == ComposeProjectTab::Yaml;
    let refreshing = if yaml_selected {
        view.compose_yaml.is_loading()
    } else {
        view.containers.phase() == super::super::state::SectionPhase::Loading
    };
    let copy_path = if yaml_selected && view.compose_yaml.selected_path().is_some() {
        let copy_ticket = ticket.clone();
        let copy_target = target.clone();
        Some(
            icon_button(
                "container-project-copy-yaml-path",
                gpui_kit::assets::IconName::Copy,
                crate::locale::text("复制 Compose YAML 路径", "Copy Compose YAML path"),
            )
            .tooltip(crate::locale::text("复制 YAML 路径", "Copy YAML path"))
            .debug_selector(|| "rr-compose-copy-yaml-path".to_owned())
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                if !view.container_action_ticket_is_current(&copy_ticket, None)
                    || view.container_host.project_tab() != ComposeProjectTab::Yaml
                    || !matches!(view.container_host.page(), ContainerHostPage::ComposeProject { target } if target == &copy_target)
                {
                    return;
                }
                if let Some(path) = view.compose_yaml.selected_path() {
                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(path.to_owned()));
                }
            })),
        )
    } else {
        None
    };
    let header = div()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .child(
            icon_button(
                "container-project-back",
                gpui_kit::assets::IconName::ArrowLeft,
                if yaml_selected {
                    crate::locale::text("返回 Compose 详情", "Back to Compose details")
                } else {
                    crate::locale::text("返回容器列表", "Back to containers")
                },
            )
            .debug_selector(|| "rr-compose-back".to_owned())
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                if view.container_action_ticket_is_current(&back_ticket, None)
                    && matches!(view.container_host.page(), ContainerHostPage::ComposeProject { target } if target == &back_target)
                {
                    if view.container_host.project_tab() == ComposeProjectTab::Yaml {
                        view.container_host.set_project_tab(ComposeProjectTab::Overview);
                        view.scroll_operations_to_top();
                        view.clear_container_project_scroll();
                    } else {
                        view.back_to_container_list();
                    }
                    cx.notify();
                }
            })),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .gap_1()
                .child({
                    let mut title = div().min_w_0().flex().items_center().gap_2().child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(theme.text_sm)
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(target.project.clone()),
                    );
                    if let Some((running, total)) = project_counts {
                        title = title.child(
                            div()
                                .flex_shrink_0()
                                .px_2()
                                .py(px(2.))
                                .rounded_full()
                                .bg(theme.materials.chrome)
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(format!("{running}/{total}")),
                        );
                    }
                    title
                })
                .child(
                    div()
                        .truncate()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(primary_path),
                ),
        )
        .child(
            icon_button(
                "container-project-refresh",
                gpui_kit::assets::IconName::RefreshCw,
                if yaml_selected {
                    crate::locale::text("刷新 Compose YAML", "Refresh Compose YAML")
                } else {
                    crate::locale::text("刷新容器列表", "Refresh containers")
                },
            )
            .debug_selector(|| "rr-compose-refresh".to_owned())
            .loading(refreshing)
            .loading_icon(gpui_kit::assets::IconName::RefreshCw)
            .disabled(view.busy || refreshing)
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                if !view.container_action_ticket_is_current(&refresh_ticket, None)
                    || !matches!(view.container_host.page(), ContainerHostPage::ComposeProject { target } if target == &refresh_target)
                {
                    return;
                }
                if view.container_host.project_tab() == ComposeProjectTab::Yaml {
                    super::compose_yaml::request_preview_for_selected_path(view, &refresh_target, &refresh_ticket, cx);
                } else {
                    view.request_container_panel_refresh(view.runtime, &refresh_ticket, cx);
                }
            })),
        )
        .child(
            icon_button(
                "container-project-inspector-yaml",
                gpui_kit::assets::IconName::FileCode,
                crate::locale::text("查看 Compose YAML", "View Compose YAML"),
            )
            .tooltip(crate::locale::text("Compose YAML", "Compose YAML"))
            .debug_selector(|| "rr-compose-yaml".to_owned())
            .selected(yaml_selected)
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                if !view.container_action_ticket_is_current(&yaml_ticket, None)
                    || !matches!(view.container_host.page(), ContainerHostPage::ComposeProject { target } if target == &yaml_target)
                {
                    return;
                }
                if view.container_host.project_tab() == ComposeProjectTab::Yaml {
                    view.container_host.set_project_tab(ComposeProjectTab::Overview);
                } else {
                    view.container_host.set_project_tab(ComposeProjectTab::Yaml);
                    super::compose_yaml::request_preview_for_selected_path(view, &yaml_target, &yaml_ticket, cx);
                }
                view.scroll_operations_to_top();
                view.clear_container_project_scroll();
                cx.notify();
            })),
        )
        .children(copy_path)
        .child(compose_actions_menu(
            &target,
            &ticket,
            view.busy || !view.compose_mutation_allowed(&target),
            view.container_mutations.compose_loading(&target),
            theme,
            "container-project-inspector-more".to_owned(),
            cx,
        ))
        .children(container_cancel_button(
            view,
            view.container_mutations.compose_executing(&target),
            "container-project-inspector-cancel".to_owned(),
            cx,
        ));
    let header = header.mb_1().border_b_1().border_color(theme.border).pb_2();

    let mut body = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .when(
            view.compact_layout && selected_tab == ComposeProjectTab::Yaml,
            |body| body.flex_1().min_h_0(),
        )
        .child(header)
        .child(tabs)
        .children(view.compact_operation_feedback(theme))
        .children(container_success_notice(view, theme));
    match (selected_tab, project) {
        (_, None) if view.containers.phase() == super::super::state::SectionPhase::Loading => {}
        (_, None) => {
            body = body.child(container_read_error_row(
                &crate::locale::text(
                    "Compose 项目已不在当前容器快照中，请返回列表刷新。",
                    "This Compose project is no longer in the current snapshot. Return to the list and refresh.",
                ),
                theme,
            ));
        }
        (ComposeProjectTab::Overview, Some(group)) => {
            body = body.child(render_project_overview(
                &target,
                &group.containers,
                &display_metadata,
                theme,
            ));
        }
        (ComposeProjectTab::Containers, Some(group)) => {
            if group.containers.is_empty() {
                body = body.child(container_group_heading(
                    &crate::locale::text("暂无容器", "No containers"),
                    &group.containers,
                    theme,
                ));
            } else {
                for container in group.containers {
                    body = body.child(render_host_container_row(
                        view,
                        container,
                        true,
                        Some(&target),
                        row_busy,
                        theme,
                        cx,
                    ));
                }
            }
        }
        (ComposeProjectTab::Yaml, Some(_)) => {
            body = body.child(super::compose_yaml::render(
                view, &target, &ticket, theme, cx,
            ));
        }
    }
    super::section_page_with_fill(
        crate::locale::text("宿主容器", "Host containers"),
        body,
        theme,
        true,
        true,
        view.compact_layout && selected_tab == ComposeProjectTab::Yaml,
    )
    .into_any_element()
}

/// Project-only presentation metadata stays outside the complete Compose execution target.
#[derive(Debug, PartialEq, Eq)]
struct ComposeProjectDisplayMetadata {
    runtime_family: &'static str,
    config_paths: Vec<String>,
    warnings: Vec<String>,
}

/// 从标签与 Compose 路径投影主版概览信息，不能污染用于去重和执行的 target。
///
/// @author kongweiguang
fn compose_project_display_metadata(
    target: &ComposeProjectTarget,
    containers: &[ContainerSummary],
) -> ComposeProjectDisplayMetadata {
    let runtime_family = containers
        .first()
        .map(|container| {
            if container
                .labels
                .iter()
                .any(|(key, _)| key.starts_with("io.podman.compose."))
            {
                "podman"
            } else if container
                .labels
                .iter()
                .any(|(key, _)| key.starts_with("com.docker.compose."))
            {
                "docker"
            } else {
                match target.runtime {
                    ContainerRuntime::Docker => "docker",
                    ContainerRuntime::Podman => "podman",
                }
            }
        })
        .unwrap_or(match target.runtime {
            ContainerRuntime::Docker => "docker",
            ContainerRuntime::Podman => "podman",
        });

    let mut config_paths = Vec::new();
    let mut path_sets = Vec::new();
    let mut working_dirs = Vec::new();
    let mut warnings = Vec::new();
    if containers.is_empty() {
        working_dirs.push(target.working_dir.clone());
        let paths = target
            .config_files
            .iter()
            .map(|path| resolve_project_compose_path(path, &target.working_dir))
            .collect::<Vec<_>>();
        if !paths.is_empty() {
            path_sets.push(paths.join("\n"));
            config_paths.extend(paths);
        }
    } else {
        for container in containers {
            let compose = container.compose.as_ref();
            let working_dir = compose
                .and_then(|compose| compose.working_dir.as_deref())
                .map(str::trim)
                .filter(|value| !value.is_empty());
            if let Some(working_dir) = working_dir
                && !working_dirs.iter().any(|current| current == working_dir)
            {
                working_dirs.push(working_dir.to_owned());
            }
            let explicit_paths = compose
                .map(|compose| {
                    compose
                        .config_paths
                        .iter()
                        .map(|path| path.trim())
                        .filter(|path| !path.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let config_files = compose
                .map(|compose| {
                    compose
                        .config_files
                        .iter()
                        .map(|path| path.trim())
                        .filter(|path| !path.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if explicit_paths.is_empty()
                && working_dir.is_none()
                && config_files
                    .iter()
                    .any(|path| !is_absolute_compose_path(path))
            {
                push_project_warning(&mut warnings, "Compose YAML 包含相对路径但缺少 workingDir");
            }
            let paths = if explicit_paths.is_empty() {
                config_files
                    .iter()
                    .map(|path| resolve_project_compose_path(path, working_dir.unwrap_or_default()))
                    .collect::<Vec<_>>()
            } else {
                explicit_paths
            };
            if !paths.is_empty() {
                let path_set = paths.join("\n");
                if !path_sets.contains(&path_set) {
                    path_sets.push(path_set);
                }
                for path in paths {
                    if !config_paths.contains(&path) {
                        config_paths.push(path);
                    }
                }
            }
        }
    }

    if working_dirs.len() > 1 {
        push_project_warning(&mut warnings, "同一 Compose 项目存在多个 workingDir");
    }
    if path_sets.len() > 1 {
        push_project_warning(&mut warnings, "同一 Compose 项目存在多组 Compose YAML 路径");
    }
    if config_paths.is_empty() {
        push_project_warning(&mut warnings, "未发现 Compose YAML 路径");
    }

    ComposeProjectDisplayMetadata {
        runtime_family,
        config_paths,
        warnings,
    }
}

/// Relative Compose paths use the remote working directory's own separator.
///
/// @author kongweiguang
fn resolve_project_compose_path(path: &str, working_dir: &str) -> String {
    if is_absolute_compose_path(path) || working_dir.trim().is_empty() {
        return path.to_owned();
    }
    let separator = if working_dir.contains('\\') {
        '\\'
    } else {
        '/'
    };
    let base = working_dir.trim_end_matches(['/', '\\']);
    let relative = path.trim_start_matches(['/', '\\']);
    format!("{base}{separator}{relative}")
}

/// Compose paths are remote POSIX or Windows values, so absoluteness is checked lexically.
///
/// @author kongweiguang
fn is_absolute_compose_path(path: &str) -> bool {
    path.starts_with('/')
        || path.starts_with('\\')
        || path.as_bytes().get(1) == Some(&b':')
            && path
                .as_bytes()
                .get(2)
                .is_some_and(|separator| matches!(separator, b'/' | b'\\'))
}

/// Keep warnings stable and avoid duplicate messages from equivalent per-container metadata.
///
/// @author kongweiguang
fn push_project_warning(warnings: &mut Vec<String>, warning: &str) {
    if !warnings.iter().any(|current| current == warning) {
        warnings.push(warning.to_owned());
    }
}

/// 项目概览展示主版可见的服务数、运行状态、运行时、目录和 YAML 路径。
///
/// @author kongweiguang
fn render_project_overview(
    target: &ComposeProjectTarget,
    containers: &[kerminal_domain::operations::ContainerSummary],
    display_metadata: &ComposeProjectDisplayMetadata,
    theme: ThemeTokens,
) -> AnyElement {
    let mut services = Vec::new();
    for container in containers {
        let service = container
            .compose
            .as_ref()
            .and_then(|compose| compose.service.as_deref())
            .filter(|service| !service.trim().is_empty())
            .unwrap_or("other");
        if !services.contains(&service) {
            services.push(service);
        }
    }
    let running = containers
        .iter()
        .filter(|container| container.status == ContainerStatus::Running)
        .count();
    let errors = containers
        .iter()
        .filter(|container| container.status == ContainerStatus::Dead)
        .count();
    let mut body = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .text_size(theme.text_xs);
    body = body.child(
        div()
            .w_full()
            .flex()
            .gap_1()
            .child(project_metric(
                crate::locale::text("服务", "Services"),
                services.len().to_string(),
                theme,
            ))
            .child(project_metric(
                crate::locale::text("运行", "Running"),
                format!("{running}/{}", containers.len()),
                theme,
            ))
            .child(project_metric(
                crate::locale::text("异常", "Errors"),
                errors.to_string(),
                theme,
            )),
    );
    body = body
        .child(project_field(
            crate::locale::text("运行时", "Runtime"),
            &format!(
                "{} / {}",
                container_runtime_label(target.runtime),
                display_metadata.runtime_family
            ),
            theme,
        ))
        .child(project_field(
            crate::locale::text("工作目录", "Working directory"),
            &target.working_dir,
            theme,
        ))
        .child(
            div().w_full().flex().flex_col().gap_1().child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(crate::locale::text("Compose YAML", "Compose YAML")),
            ),
        );
    for path in &display_metadata.config_paths {
        body = body.child(
            div()
                .w_full()
                .min_w_0()
                .text_size(theme.text_xs)
                .child(path.clone()),
        );
    }
    if !display_metadata.warnings.is_empty() {
        body = body.child(container_read_error_row(
            &display_metadata.warnings.join(" / "),
            theme,
        ));
    }
    body.into_any_element()
}

/// 项目指标使用相同底色和紧凑的标签/数值顺序，保持窄栏可比较。
///
/// @author kongweiguang
fn project_metric(
    label: impl Into<gpui_kit::SharedString>,
    value: String,
    theme: ThemeTokens,
) -> AnyElement {
    let label = label.into();
    div()
        .min_w_0()
        .flex_1()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .rounded_md()
        .bg(theme.sidebar)
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(label),
        )
        .child(div().truncate().text_size(theme.text_sm).child(value))
        .into_any_element()
}

/// 详情字段保持单列，避免运行时路径或长配置路径挤压三列指标。
///
/// @author kongweiguang
fn project_field(
    label: impl Into<gpui_kit::SharedString>,
    value: &str,
    theme: ThemeTokens,
) -> AnyElement {
    let label = label.into();
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(label),
        )
        .child(
            div()
                .min_w_0()
                .text_size(theme.text_xs)
                .child(value.to_owned()),
        )
        .into_any_element()
}

/// 容器 inspector 的 tab 和快照均绑定到目标；页头按 main 的返回/对象动作排布，刷新与 tabs 同排。
///
/// @author kongweiguang
fn render_host_container(
    view: &mut OperationsView,
    target: ContainerTarget,
    theme: ThemeTokens,
    cx: &mut Context<OperationsView>,
) -> AnyElement {
    let ticket = view.container_action_ticket(Some(target.clone()));
    let container = view
        .containers
        .data()
        .and_then(|rows| rows.iter().find(|row| row.target == target))
        .cloned();
    let selected_tab = view.container_host.container_tab();
    let selected_tab_loading = match selected_tab {
        HostContainerTab::Details => {
            view.container_inspect.phase() == super::super::DetailReadPhase::Loading
        }
        HostContainerTab::Stats => {
            view.container_output.phase() == super::super::DetailReadPhase::Loading
        }
    };
    let back_ticket = ticket.clone();
    let back_target = target.clone();
    let back_origin = view.container_host.container_origin().cloned();
    let back_label = if back_origin.is_some() {
        crate::locale::text("返回 Compose 详情", "Back to Compose details")
    } else {
        crate::locale::text("返回容器列表", "Back to containers")
    };
    let mut header = div()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .child(
            icon_button(
                "container-inspector-back",
                gpui_kit::assets::IconName::ArrowLeft,
                back_label,
            )
            .debug_selector(|| "rr-container-back".to_owned())
            .with_size(Size::Medium)
            .h_8()
            .w_8()
            .rounded_lg()
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                if !view.container_action_ticket_is_current(&back_ticket, Some(&back_target))
                    || !matches!(
                        view.container_host.page(),
                        ContainerHostPage::Container { target } if target == &back_target
                    )
                {
                    return;
                }
                if let Some(project) = back_origin.clone() {
                    view.container_host.return_to_project(project);
                    view.restore_container_project_scroll();
                } else {
                    view.back_to_container_list();
                }
                cx.notify();
            })),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .truncate()
                        .text_size(theme.text_sm)
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .child(container.as_ref().map_or_else(
                            || target.container_id.clone(),
                            |container| container.name.clone(),
                        )),
                )
                .when_some(container.as_ref(), |this, container| {
                    this.child(
                        div()
                            .truncate()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(if container.ports.is_empty() {
                                container_status_group_label(container.status).to_string()
                            } else {
                                format!(
                                    "{} · {}",
                                    container_status_group_label(container.status),
                                    container.ports.join(", ")
                                )
                            }),
                    )
                }),
        );
    if let Some(container) = container.as_ref() {
        let can_enter = container.status == ContainerStatus::Running;
        let action = if can_enter {
            None
        } else if can_run_host_container_action(container.status, ContainerAction::Start) {
            Some(ContainerAction::Start)
        } else {
            None
        };
        if can_enter {
            let terminal_target = target.clone();
            let terminal_ticket = ticket.clone();
            header = header.child(
                icon_button(
                    "container-inspector-terminal",
                    gpui_kit::assets::IconName::Terminal,
                    crate::locale::text("进入容器终端", "Open container terminal"),
                )
                .with_size(Size::Medium)
                .secondary()
                .h_8()
                .w_8()
                .rounded_lg()
                .disabled(view.busy || view.container_write_scope_blocked())
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    view.request_container_exec(terminal_target.clone(), &terminal_ticket, cx);
                })),
            );
        } else if let Some(action) = action {
            let start_target = target.clone();
            let start_ticket = ticket.clone();
            header = header.child(
                icon_button(
                    "container-inspector-start",
                    gpui_kit::assets::IconName::Play,
                    crate::locale::text("启动容器", "Start container"),
                )
                .with_size(Size::Medium)
                .secondary()
                .h_8()
                .w_8()
                .rounded_lg()
                .disabled(view.busy || view.container_write_scope_blocked())
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    view.request_container_mutation(
                        ContainerMutationRequest {
                            target: start_target.clone(),
                            action,
                            force: false,
                        },
                        &start_ticket,
                        cx,
                    );
                })),
            );
        }
        header = header.child(container_actions_menu(
            container,
            &ticket,
            view.busy
                || view.container_write_scope_blocked()
                || view.containers.phase() != super::super::state::SectionPhase::Ready,
            view.container_mutations.container_loading(&target),
            theme,
            "container-inspector-more".to_owned(),
            cx,
        ));
        header = header.children(container_cancel_button(
            view,
            view.container_mutations.container_executing(&target),
            "container-inspector-cancel".to_owned(),
            cx,
        ));
    }
    header = header.mb_1().border_b_1().border_color(theme.border).pb_2();

    let inspector_theme = theme.for_inspector();
    let mut tabs = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_1()
        .max_w(px(352.))
        .h_8()
        .p_0p5()
        .rounded_xl()
        .bg(inspector_theme.secondary);
    for (tab, label) in [
        (
            HostContainerTab::Details,
            crate::locale::text("详情", "Details"),
        ),
        (
            HostContainerTab::Stats,
            crate::locale::text("监控", "Stats"),
        ),
    ] {
        let tab_target = target.clone();
        let tab_ticket = ticket.clone();
        let selected = selected_tab == tab;
        let button = Button::new(format!("container-inspector-tab-{tab:?}"))
            .compact()
            .ghost()
            .debug_selector(move || format!("rr-container-tab-{tab:?}"))
            .selected(selected)
            .h_7()
            .flex_1()
            .rounded_lg()
            .px_2()
            .text_size(theme.text_xs)
            .label(label)
            .text_color(if selected {
                inspector_theme.foreground
            } else {
                inspector_theme.muted
            })
            .when(selected, |button| button.bg(theme.panel).shadow_sm())
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                if !view.container_action_ticket_is_current(&tab_ticket, Some(&tab_target)) {
                    return;
                }
                view.container_host.set_container_tab(tab);
                view.scroll_operations_to_top();
                view.request_container_read(
                    tab_target.clone(),
                    match tab {
                        HostContainerTab::Details => ContainerReadKind::Inspect,
                        HostContainerTab::Stats => ContainerReadKind::Stats,
                    },
                    &tab_ticket,
                    cx,
                );
            }));
        tabs = tabs.child(button);
    }
    let refresh_target = target.clone();
    let refresh_ticket = ticket.clone();
    let list_ticket = view.container_action_ticket(None);
    let needs_list_refresh = view.container_write_scope_blocked() || container.is_none();
    let list_loading = view.containers.phase() == super::super::state::SectionPhase::Loading;
    let refresh_label = if needs_list_refresh {
        crate::locale::text("刷新容器列表", "Refresh container list")
    } else {
        crate::locale::text("刷新容器信息", "Refresh container info")
    };
    let refresh_button = icon_button(
        "container-inspector-refresh",
        gpui_kit::assets::IconName::RefreshCw,
        refresh_label.clone(),
    )
    .debug_selector(|| "rr-container-refresh".to_owned())
    .h_8()
    .w_8()
    .rounded_lg()
    .tooltip(refresh_label)
    .loading(if needs_list_refresh {
        list_loading
    } else {
        selected_tab_loading
    })
    .loading_icon(gpui_kit::assets::IconName::RefreshCw)
    .disabled(if needs_list_refresh {
        view.busy || list_loading
    } else {
        container.is_none() || selected_tab_loading
    })
    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
        if view.container_write_scope_blocked()
            || !view
                .containers
                .data()
                .is_some_and(|rows| rows.iter().any(|row| row.target == refresh_target))
        {
            view.request_container_panel_refresh(view.runtime, &list_ticket, cx);
            return;
        }
        if !view.container_action_ticket_is_current(&refresh_ticket, Some(&refresh_target)) {
            return;
        }
        view.request_container_read(
            refresh_target.clone(),
            match view.container_host.container_tab() {
                HostContainerTab::Details => ContainerReadKind::Inspect,
                HostContainerTab::Stats => ContainerReadKind::Stats,
            },
            &refresh_ticket,
            cx,
        );
    }));
    let tabs_row = div()
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .justify_end()
        .gap_2()
        .child(tabs)
        .child(refresh_button)
        .border_b_1()
        .border_color(theme.border)
        .pb_2();

    let mut body = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .child(header)
        .child(tabs_row)
        .children(view.compact_operation_feedback(theme))
        .children(container_success_notice(view, theme));
    if container.is_none() {
        if view.containers.phase() != super::super::state::SectionPhase::Loading {
            body = body.child(container_read_error_row(
                &crate::locale::text(
                    "所选容器已不存在，请返回列表刷新状态。",
                    "The selected container no longer exists. Return to the list and refresh.",
                ),
                theme,
            ));
        }
    } else {
        match selected_tab {
            HostContainerTab::Details => {
                if let Some(message) = view.container_inspect.feedback() {
                    body = body.child(container_read_error_row(message, theme));
                }
                if let Some(details) = view
                    .container_inspect
                    .data()
                    .filter(|details| details.target == target)
                {
                    body = body.child(super::container_inspect(details, theme));
                }
            }
            HostContainerTab::Stats => {
                if let Some(message) = view.container_output.feedback() {
                    body = body.child(container_read_error_row(message, theme));
                }
                let stats = view
                    .container_output
                    .data()
                    .and_then(|output| output.stats())
                    .filter(|stats| stats.target == target);
                if stats.is_some()
                    || view.container_output.phase() == super::super::DetailReadPhase::Ready
                {
                    body = body.child(container_stats_panel(stats, theme));
                }
            }
        }
    }
    super::section_page(
        crate::locale::text("宿主容器", "Host containers"),
        body,
        theme,
        true,
        true,
    )
    .into_any_element()
}

/// 监控面板保留全部原始字段，正文随页面滚动并在窄栏按内容折行。
///
/// @author kongweiguang
fn container_stats_panel(stats: Option<&ContainerStats>, theme: ThemeTokens) -> AnyElement {
    let inspector_theme = theme.for_inspector();
    let display = container_stats_display(stats);
    let memory_label = crate::locale::text("内存", "Memory").to_string();
    let network_label = crate::locale::text("网络", "Network").to_string();
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .w_full()
                .grid()
                .grid_cols(2)
                .gap_2()
                .child(container_stats_metric(
                    "CPU",
                    display.cpu_percent.clone(),
                    theme,
                ))
                .child(container_stats_metric(
                    &memory_label,
                    display.memory_percent.clone(),
                    theme,
                ))
                .child(container_stats_metric("PIDs", display.pids.clone(), theme))
                .child(container_stats_metric(
                    &network_label,
                    display.network_io.clone(),
                    theme,
                )),
        )
        .child(super::container_detail_row(
            crate::locale::text("内存使用", "Memory usage").to_string(),
            display.memory_usage,
            theme,
        ))
        .child(super::container_detail_row(
            "Block IO",
            display.block_io,
            theme,
        ))
        .child(
            div()
                .id("container-inspector-stats-raw")
                .debug_selector(|| "rr-container-stats-raw".to_owned())
                .w_full()
                .min_w_0()
                .p_2()
                .rounded_xl()
                .bg(inspector_theme.secondary)
                .text_size(px(11.))
                .font_family("monospace")
                .whitespace_normal()
                .text_color(theme.muted)
                .child(if display.raw.trim().is_empty() {
                    crate::locale::text("暂无 stats 数据。", "No stats data available.")
                } else {
                    super::add_container_detail_soft_breaks(&format_container_stats_raw(
                        &display.raw,
                    ))
                    .into()
                }),
        )
        .into_any_element()
}

/// 仅对有效 JSON 做可读格式化；异常原文保持完整，避免诊断信息被解析失败吞掉。
///
/// @author kongweiguang
fn format_container_stats_raw(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or_else(|_| raw.to_owned())
}

/// stats 的缺失值统一显示为横线，保持 main 的指标排序且不解析原始 JSON。
///
/// @author kongweiguang
fn container_stats_display(stats: Option<&ContainerStats>) -> ContainerStatsDisplay {
    let value = |field: Option<&String>| field.cloned().unwrap_or_else(|| "-".to_owned());
    ContainerStatsDisplay {
        cpu_percent: value(stats.and_then(|stats| stats.cpu_percent.as_ref())),
        memory_percent: value(stats.and_then(|stats| stats.memory_percent.as_ref())),
        pids: value(stats.and_then(|stats| stats.pids.as_ref())),
        network_io: value(stats.and_then(|stats| stats.network_io.as_ref())),
        memory_usage: value(stats.and_then(|stats| stats.memory_usage.as_ref())),
        block_io: value(stats.and_then(|stats| stats.block_io.as_ref())),
        raw: stats.map_or_else(String::new, |stats| stats.raw.clone()),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ContainerStatsDisplay {
    cpu_percent: String,
    memory_percent: String,
    pids: String,
    network_io: String,
    memory_usage: String,
    block_io: String,
    raw: String,
}

/// 监控卡片沿用 main 的中性表面、圆角和单行等宽值，窄栏中长值截断而不撑高卡片。
///
/// @author kongweiguang
fn container_stats_metric(label: &str, value: String, theme: ThemeTokens) -> AnyElement {
    let inspector_theme = theme.for_inspector();
    div()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .px_2p5()
        .py_2()
        .rounded_xl()
        .bg(inspector_theme.secondary)
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme.muted)
                .child(label.to_owned()),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_family("monospace")
                .text_size(px(14.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .child(value),
        )
        .into_any_element()
}

/// runtime 名称使用领域枚举，不从容器状态或标题解析。
///
/// @author kongweiguang
fn container_runtime_label(runtime: ContainerRuntime) -> &'static str {
    match runtime {
        ContainerRuntime::Docker => "Docker",
        ContainerRuntime::Podman => "Podman",
    }
}

/// 容器终端只读页按当前分页读取唯一 target；正文可在窄栏收缩，终端退出仍保留原目标快照。
///
/// @author kongweiguang
fn render_container_info(
    view: &mut OperationsView,
    target: &kerminal_domain::operations::ContainerTarget,
    terminal_running: bool,
    theme: ThemeTokens,
    cx: &mut Context<OperationsView>,
) -> AnyElement {
    let ticket = view.container_action_ticket(Some(target.clone()));
    let selected = view.container_info_tab;
    let mut tabs = div().w_full().min_w_0().flex().flex_wrap().gap_1();
    for (kind, zh, en, id) in [
        (ContainerReadKind::Inspect, "概览", "Overview", "overview"),
        (ContainerReadKind::Stats, "资源", "Resources", "resources"),
        (ContainerReadKind::Logs, "日志", "Logs", "logs"),
    ] {
        let target = target.clone();
        let ticket = ticket.clone();
        let button = Button::new(format!("container-info-tab-{id}"))
            .compact()
            .label(crate::locale::text(zh, en))
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                view.request_container_read(target.clone(), kind, &ticket, cx)
            }));
        tabs = tabs.child(if selected == kind {
            button.primary()
        } else {
            button.ghost()
        });
    }
    let full_container_id = target.container_id.clone();
    let target_id_tooltip = crate::locale::text(
        format!("容器 ID：{full_container_id}"),
        format!("Container ID: {full_container_id}"),
    );
    let target_id_accessible = target_id_tooltip.clone();
    let visible_container_prefix = target.container_id.chars().take(12).collect::<String>();
    let visible_container_id = if target.container_id.chars().count() > 12 {
        format!("{visible_container_prefix}…")
    } else {
        visible_container_prefix
    };
    let mut body = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .child(tabs)
        .child(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded_md()
                .bg(theme.sidebar)
                .child(
                    div()
                        .id("container-info-target-id")
                        .w_full()
                        .min_w_0()
                        .text_size(px(13.))
                        .truncate()
                        .child(visible_container_id)
                        .tooltip(move |window, cx| {
                            Tooltip::new(target_id_tooltip.clone()).build(window, cx)
                        })
                        .aria_label(target_id_accessible),
                )
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(format!("{:?}", target.runtime)),
                ),
        );
    if !terminal_running {
        body = body.child(
            div()
                .w_full()
                .p_2()
                .rounded_md()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "容器终端已结束；此页仍绑定原容器，可刷新查看最后状态。",
                    "The container terminal has ended. This page remains bound to the same container and can still be refreshed.",
                )),
        );
    }

    match selected {
        ContainerReadKind::Inspect => {
            if let Some(message) = view.container_inspect.feedback() {
                body = body.child(container_read_error_row(message, theme));
            }
            if let Some(details) = view.container_inspect.data() {
                let details = details.clone();
                body = body.child(super::container_inspect(&details, theme));
                let copy = format!("{}\n{}\n{}", details.name, details.id, details.image);
                body = body.child(copy_button(
                    copy,
                    "container-info-copy-overview",
                    crate::locale::text("复制容器概览", "Copy container overview"),
                ));
            }
        }
        ContainerReadKind::Stats | ContainerReadKind::Logs => {
            if let Some(message) = view.container_output.feedback() {
                body = body.child(container_read_error_row(message, theme));
            }
            if let Some(output) = view.container_output.data() {
                let output = output.display_text();
                let no_logs = selected == ContainerReadKind::Logs && output.trim().is_empty();
                body = body.child(
                    div()
                        .w_full()
                        .p_2()
                        .rounded_md()
                        .bg(theme.sidebar)
                        .text_size(theme.text_xs)
                        .text_color(if no_logs {
                            theme.muted
                        } else {
                            theme.foreground
                        })
                        .child(if no_logs {
                            crate::locale::text("暂无日志", "No logs available.")
                        } else {
                            output.clone().into()
                        }),
                );
                if !no_logs {
                    body = body.child(copy_button(
                        output,
                        "container-info-copy-output",
                        crate::locale::text("复制容器输出", "Copy container output"),
                    ));
                }
            }
        }
    }

    super::section_page(
        crate::locale::text("容器信息", "Container info"),
        body,
        theme,
        true,
        true,
    )
    .into_any_element()
}

/// 状态标签使用主题令牌并保留原始状态文字供用户识别外部变化。
///
/// @author kongweiguang
fn status_pill(text: String, status: ContainerStatus, theme: ThemeTokens) -> impl IntoElement {
    let (color, background) = match status {
        ContainerStatus::Running => (theme.success, theme.success.opacity(0.12)),
        ContainerStatus::Dead => (theme.error, theme.error.opacity(0.12)),
        ContainerStatus::Paused | ContainerStatus::Restarting => {
            (theme.warning, theme.warning.opacity(0.12))
        }
        ContainerStatus::Exited | ContainerStatus::Created | ContainerStatus::Unknown => {
            (theme.muted, theme.muted.opacity(0.12))
        }
    };
    div()
        .px_2()
        .py(px(2.))
        .rounded_full()
        .text_size(px(11.))
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(color)
        .bg(background)
        .child(text)
}

/// 只读页错误使用主题错误色并在窄栏显式折行，避免完整原因被裁切后无法判断如何重试。
///
/// @author kongweiguang
fn container_read_error_row(message: &str, theme: ThemeTokens) -> impl IntoElement {
    let display_message = super::add_container_detail_soft_breaks(message);
    div()
        .w_full()
        .min_w_0()
        .p_2()
        .rounded_md()
        .bg(theme.sidebar)
        .text_size(theme.text_xs)
        .text_color(theme.error)
        .child(display_message)
}

/// 复制按钮贴近已加载内容并标明其范围，避免图标隐藏具体复制对象。
///
/// @author kongweiguang
fn copy_button(
    value: String,
    id: &str,
    accessible_label: gpui_kit::SharedString,
) -> impl IntoElement {
    icon_button(
        id.to_owned(),
        gpui_kit::assets::IconName::Copy,
        accessible_label,
    )
    .on_click(move |_, _, cx| {
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(value.clone()));
    })
}

#[cfg(test)]
#[path = "../../../../tests/unit/operations_render_container.rs"]
mod container_render_tests;
