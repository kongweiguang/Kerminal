//! 分屏目标选择弹层；只负责搜索、显式选择与关闭，不启动连接或变更 workspace。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyView, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable as _,
    IntoElement, Render, SharedString, Subscription, Window,
    component::button::{Button, ButtonVariants as _},
    component::input::{InputEvent, InputState},
    prelude::*,
    px,
};
use kerminal_application::{connections::management::HostGroup, workspace::WorkspaceTargetRef};
use kerminal_domain::identity::{PaneId, SessionId, TabId};

use crate::features::{
    connections::HostNavigation,
    host_picker::{
        HostPickerGroup, HostPickerList, HostPickerListMessage, HostPickerOption,
        HostPickerPopoverProjection, HostPickerPopoverRequest,
    },
    workspace_controller::WorkspaceSplitTargetOption,
};

use super::{PendingPaneFocusIntent, RootView};

/// 菜单打开时冻结原 tab/pane/session 与分屏方向；确认时 Root 再校验此身份仍 Ready。
#[derive(Clone, Debug, PartialEq)]
pub(super) struct SplitTargetPickerRequest {
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub source_session_id: SessionId,
    pub direction: kerminal_domain::workspace::SplitDirection,
    pub before: bool,
    pub request_generation: u64,
}

impl SplitTargetPickerRequest {
    /// 选择期间 pane 被替换、关闭或失去原 tab owner 时拒绝迟到确认，不把动作重定向到新会话。
    ///
    /// @author kongweiguang
    pub(super) fn matches_source(
        &self,
        tab_id: &TabId,
        pane_id: &PaneId,
        session_id: Option<&SessionId>,
        ready: bool,
    ) -> bool {
        ready
            && &self.tab_id == tab_id
            && &self.pane_id == pane_id
            && session_id == Some(&self.source_session_id)
    }
}

/// 只把 Root 已捕获的来源和所选目标回传；资源生命周期留在唯一 Root restore 路径。
#[derive(Clone, Debug, PartialEq)]
pub(super) enum SplitTargetPickerEvent {
    Confirm {
        request: SplitTargetPickerRequest,
        target: WorkspaceSplitTargetOption,
    },
}

/// 选择弹层不持有连接配置正文，目标由 HostNavigation 的当前分组投影即时重算。
pub(super) struct SplitTargetPicker {
    navigation: Option<Entity<HostNavigation>>,
    search: Entity<InputState>,
    _search_subscription: Subscription,
    query: String,
    request: Option<SplitTargetPickerRequest>,
    status: Option<SharedString>,
    return_focus: Option<FocusHandle>,
}

impl EventEmitter<SplitTargetPickerEvent> for SplitTargetPicker {}

impl SplitTargetPicker {
    /// 搜索输入和菜单共享焦点陷阱；本地和远程均沿用主机目录，内部存储类型不成为第二类导航概念。
    ///
    /// @author kongweiguang
    pub(super) fn new(
        navigation: Option<Entity<HostNavigation>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text(
                "搜索主机、地址、协议或分组",
                "Search hosts, addresses, protocols, or groups",
            ))
        });
        let search_subscription = cx.subscribe(&search, |picker, input, event, cx| {
            if matches!(event, InputEvent::Change) {
                picker.query = input.read(cx).value().to_string();
                picker.status = None;
                cx.notify();
            }
        });
        Self {
            navigation,
            search,
            _search_subscription: search_subscription,
            query: String::new(),
            request: None,
            status: None,
            return_focus: None,
        }
    }

    /// 右键来源保持原 pane；打开弹层本身不激活工作区或创建任何连接资源。
    ///
    /// @author kongweiguang
    pub(super) fn open(
        &mut self,
        request: SplitTargetPickerRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.request.is_some() {
            return false;
        }
        self.return_focus = window.focused(cx);
        self.request = Some(request);
        self.query.clear();
        self.status = None;
        self.search.update(cx, |input, cx| {
            input.set_value(String::new(), window, cx);
            input.focus(window, cx);
        });
        cx.notify();
        true
    }

    /// 关闭只返回打开前焦点；Root 负责恢复它并决定是否按确认事件启动新 pane。
    ///
    /// @author kongweiguang
    pub(super) fn close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<FocusHandle> {
        self.request = None;
        self.query.clear();
        self.status = None;
        self.search
            .update(cx, |input, cx| input.set_value(String::new(), window, cx));
        cx.notify();
        self.return_focus.take()
    }

    /// Root 只处理当前弹层的确认；取消由锚点携带完整来源与代次直接关闭，避免重复事件路径。
    ///
    /// @author kongweiguang
    pub(super) fn matches_request(&self, request: &SplitTargetPickerRequest) -> bool {
        self.request.as_ref() == Some(request)
    }

    /// Root 将弹层只投影到捕获请求对应的 pane 与方向按钮。
    ///
    /// @author kongweiguang
    pub(super) fn popover_request(&self) -> Option<HostPickerPopoverRequest> {
        self.request
            .as_ref()
            .map(|request| HostPickerPopoverRequest {
                tab_id: request.tab_id.clone(),
                pane_id: request.pane_id.clone(),
                source_session_id: request.source_session_id.clone(),
                direction: request.direction,
                before: request.before,
                generation: request.request_generation,
            })
    }

    /// Popover 将焦点放在持久搜索输入上，输入法状态由 picker 自己保留。
    ///
    /// @author kongweiguang
    pub(super) fn search_focus_handle(&self, cx: &App) -> FocusHandle {
        self.search.read(cx).focus_handle(cx)
    }

    /// 根层在模态期间阻断应用快捷键与终端输入，关闭的零尺寸实体不占有输入。
    ///
    /// @author kongweiguang
    pub(super) fn is_open(&self) -> bool {
        self.request.is_some()
    }

    /// 只在当前目录仍包含同一 typed target 时发出选择，拒绝配置重载后的旧行目标。
    ///
    /// @author kongweiguang
    fn select_target(&mut self, selected_target: WorkspaceTargetRef, cx: &mut Context<Self>) {
        let Some(request) = self.request.clone() else {
            return;
        };
        let options = self.options(cx);
        let Some(target) = current_selected_option(&options, &selected_target) else {
            self.status = Some(crate::locale::text(
                "目标列表已变化，请重新选择。",
                "The target list changed. Select a target again.",
            ));
            cx.notify();
            return;
        };
        cx.emit(SplitTargetPickerEvent::Confirm { request, target });
    }

    /// 每次 render 从导航最新目录投影候选，不在 picker 保存完整连接配置或凭据。
    ///
    /// @author kongweiguang
    fn options(&self, cx: &Context<Self>) -> Vec<WorkspaceSplitTargetOption> {
        self.navigation
            .as_ref()
            .map(|navigation| navigation.read(cx).split_target_options(""))
            .unwrap_or_default()
    }

    /// 分组标题和 loading/error 状态只读既有导航快照，不触发额外请求或回退目标。
    ///
    /// @author kongweiguang
    fn directory_state(&self, cx: &Context<Self>) -> (Vec<HostGroup>, bool, Option<String>) {
        self.navigation.as_ref().map_or_else(
            || (Vec::new(), false, None),
            |navigation| {
                let navigation = navigation.read(cx);
                (
                    navigation.split_target_groups(),
                    navigation.split_target_loading(),
                    navigation.split_target_status(),
                )
            },
        )
    }

    /// 将导航目录映射为通用列表投影；搜索包含协议摘要与真实分组名。
    ///
    /// @author kongweiguang
    fn render_options(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::ThemeTokens::current(window, cx);
        let (groups, loading, directory_status) = self.directory_state(cx);
        let options = self.options(cx);
        let list_options = options
            .iter()
            .map(|option| HostPickerOption {
                key: target_option_key(&option.target).into(),
                target: option.clone(),
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
        let picker = cx.entity().clone();
        let mut list = HostPickerList::new(
            "split-target-picker-options",
            list_options,
            move |selected, app_window, app| {
                let selected_target = selected.target;
                picker.update(app, |picker, cx| picker.select_target(selected_target, cx));
                app_window.prevent_default();
            },
        )
        .search_input(self.search.clone())
        .query(self.query.clone())
        .groups(list_groups)
        .theme(theme);

        let has_directory_error = directory_status.is_some();
        if loading {
            list = list.message(HostPickerListMessage::Loading(crate::locale::text(
                "正在加载连接目录…",
                "Loading the connection directory…",
            )));
        } else if let Some(status) = directory_status {
            list = list.message(HostPickerListMessage::Error(status.into()));
        } else if options.is_empty() && self.query.trim().is_empty() {
            list = list.message(HostPickerListMessage::Empty(crate::locale::text(
                "没有可用于分屏的本地、SSH、Telnet 或 Serial 主机。",
                "No Local, SSH, Telnet, or Serial split targets are available.",
            )));
        }
        let mut has_footer = false;
        let mut footer = gpui_kit::div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap_2();
        if let Some(status) = self.status.clone() {
            footer = footer.child(
                gpui_kit::div()
                    .text_size(theme.text_xs)
                    .text_color(theme.warning)
                    .child(status),
            );
            has_footer = true;
        }
        if has_directory_error
            && !loading
            && let Some(navigation) = self.navigation.clone()
        {
            footer = footer.child(
                Button::new("split-target-picker-retry")
                    .compact()
                    .ghost()
                    .label(crate::locale::text("重试加载", "Retry loading"))
                    .on_click(move |_, window, app| {
                        navigation.update(app, |navigation, cx| navigation.reload(cx));
                        window.prevent_default();
                    }),
            );
            has_footer = true;
        }
        if has_footer {
            list = list.footer(footer);
        }
        list
    }
}

/// 确认只接受当前目录仍可见的同一 target ref，拒绝搜索变化后的旧选择。
///
/// @author kongweiguang
fn current_selected_option(
    options: &[WorkspaceSplitTargetOption],
    selected_target: &WorkspaceTargetRef,
) -> Option<WorkspaceSplitTargetOption> {
    options
        .iter()
        .find(|option| &option.target == selected_target)
        .cloned()
}

impl Render for SplitTargetPicker {
    /// 选择器只渲染主机列表内容，布局、方向和关闭由锚定 Popover 与 Root 拥有。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.request.is_none() {
            return gpui_kit::div()
                .id("split-target-picker-closed")
                .w(px(0.))
                .h(px(0.))
                .into_any_element();
        }
        self.render_options(window, cx).into_any_element()
    }
}
impl RootView {
    /// 只为所属 tab 创建锚点投影；popover 内容复用同一 picker Entity 和持久搜索焦点。
    ///
    /// @author kongweiguang
    pub(super) fn split_target_picker_projection(
        &self,
        tab_id: &TabId,
        cx: &App,
    ) -> Option<HostPickerPopoverProjection> {
        let picker = self.split_target_picker.clone();
        let picker_state = picker.read(cx);
        let request = picker_state.popover_request()?;
        if &request.tab_id != tab_id {
            return None;
        }
        Some(HostPickerPopoverProjection {
            request,
            content: AnyView::from(picker.clone()),
            focus_handle: picker_state.search_focus_handle(cx),
        })
    }

    /// 只允许仍在原 tab 中、拥有真实 Ready session 的终端 pane 发起选择器请求。
    ///
    /// @author kongweiguang
    pub(super) fn open_split_target_picker(
        &mut self,
        tab_id: kerminal_domain::identity::TabId,
        pane_id: kerminal_domain::identity::PaneId,
        direction: kerminal_domain::workspace::SplitDirection,
        before: bool,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        if window.has_active_prompt()
            || self.preferences_modal_open
            || self.connections_modal_open
            || self.connection_prompt_modal_open
            || self.root_confirmation_owner.is_some()
            || self.snippet_editor_modal_open
            || self.pending_quit_intent.is_some()
        {
            self.status = Some(crate::locale::text(
                "请先关闭当前弹层，再发起分屏。",
                "Close the current modal before starting a split.",
            ));
            cx.notify();
            return;
        }
        if self.host_navigation.is_none() {
            self.status = Some(crate::locale::text(
                "连接目录尚不可用，无法选择分屏目标。",
                "The connection directory is unavailable, so no split target can be selected.",
            ));
            cx.notify();
            return;
        }
        let source = self.workspace_runtime.pane(&pane_id);
        let Some(source) = source.filter(|pane| {
            pane.tab_id == tab_id
                && pane.phase == super::workspace_runtime::RuntimePanePhase::Ready
                && pane.session_id.is_some()
        }) else {
            self.status = Some(crate::locale::text(
                "分屏来源已失效或尚未就绪。",
                "The split source is unavailable or not ready.",
            ));
            cx.notify();
            return;
        };
        let layout_contains_source = self
            .workspace_controller
            .as_ref()
            .is_some_and(|controller| {
                controller.snapshot().terminal_tabs.iter().any(|tab| {
                    tab.id() == &tab_id
                        && tab
                            .terminal_layout()
                            .is_some_and(|layout| layout.contains_pane(&pane_id))
                })
            });
        let Some(source_session_id) = source.session_id.clone().filter(|_| layout_contains_source)
        else {
            self.status = Some(crate::locale::text(
                "分屏来源已从当前工作区移除。",
                "The split source is no longer in the current workspace.",
            ));
            cx.notify();
            return;
        };
        if self.center_panel != super::CenterPanel::Terminal {
            self.status = Some(crate::locale::text(
                "请先返回终端工作区，再选择分屏目标。",
                "Return to the terminal workspace before choosing a split target.",
            ));
            cx.notify();
            return;
        }
        self.split_target_picker_generation =
            self.split_target_picker_generation.wrapping_add(1).max(1);
        let request = SplitTargetPickerRequest {
            tab_id,
            pane_id,
            source_session_id,
            direction,
            before,
            request_generation: self.split_target_picker_generation,
        };
        let opened = self
            .split_target_picker
            .update(cx, |picker, cx| picker.open(request, window, cx));
        if !opened {
            self.status = Some(crate::locale::text(
                "目标选择器已打开，请先完成或取消当前选择。",
                "A target picker is already open. Confirm or cancel it first.",
            ));
            cx.notify();
        }
    }

    /// 只接受仍对应当前 popover 请求的关闭事件，旧方向或旧代次不能取消新选择。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_split_target_picker_request(
        &mut self,
        pane_id: &PaneId,
        direction: kerminal_domain::workspace::SplitDirection,
        before: bool,
        request_generation: u64,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        let matches = self
            .split_target_picker
            .read(cx)
            .popover_request()
            .is_some_and(|request| {
                &request.pane_id == pane_id
                    && request.direction == direction
                    && request.before == before
                    && request.generation == request_generation
            });
        if !matches {
            return;
        }
        let picker_had_focus = self
            .split_target_picker
            .read(cx)
            .search_focus_handle(cx)
            .is_focused(window);
        let return_focus = self
            .split_target_picker
            .update(cx, |picker, cx| picker.close(window, cx));
        if picker_had_focus && let Some(focus) = return_focus {
            window.focus(&focus, cx);
        }
    }

    /// 标签、来源会话或终端页面切换会卸载锚点；只取消旧请求，不恢复已离开的 pane 焦点。
    ///
    /// @author kongweiguang
    pub(super) fn reconcile_split_target_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        let Some(request) = self.split_target_picker.read(cx).popover_request() else {
            return;
        };
        let active_tab = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.snapshot().active_tab_id.clone());
        let source_is_current = self
            .workspace_runtime
            .pane(&request.pane_id)
            .is_some_and(|pane| {
                pane.tab_id == request.tab_id
                    && pane.persisted.id == request.pane_id
                    && pane.session_id.as_ref() == Some(&request.source_session_id)
                    && pane.phase == super::workspace_runtime::RuntimePanePhase::Ready
            });
        let layout_contains_source = self
            .workspace_controller
            .as_ref()
            .is_some_and(|controller| {
                controller.snapshot().terminal_tabs.iter().any(|tab| {
                    tab.id() == &request.tab_id
                        && tab
                            .terminal_layout()
                            .is_some_and(|layout| layout.contains_pane(&request.pane_id))
                })
            });
        if active_tab.as_ref() == Some(&request.tab_id)
            && source_is_current
            && layout_contains_source
            && self.center_panel == super::CenterPanel::Terminal
            && self.pending_quit_intent.is_none()
        {
            return;
        }
        let still_same_request = self
            .split_target_picker
            .read(cx)
            .popover_request()
            .is_some_and(|current| current == request);
        if still_same_request {
            // 来源或标签已经变化，旧 pane 的焦点快照不再是可安全恢复的目标。
            self.split_target_picker
                .update(cx, |picker, cx| picker.close(window, cx));
        }
    }

    /// 分屏复用 CreatePane ACK；显式 ClosePane 捕获受限焦点交接。可变 Window 继续传到资源入口检查真实 Dialog。
    ///
    /// @author kongweiguang
    pub(super) fn dispatch_split_resource_operation(
        &mut self,
        operation: crate::features::workspace_controller::WorkspaceResourceOperation,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        let focus_target = match &operation {
            crate::features::workspace_controller::WorkspaceResourceOperation::CreatePane {
                tab_id,
                pane,
                ..
            } => Some((tab_id.clone(), pane.id.clone())),
            _ => None,
        };
        self.handle_workspace_resource_operation_from_window(operation, None, window, cx);
        if let Some((tab_id, pane_id)) = focus_target
            && self.workspace_runtime.pane(&pane_id).is_some_and(|pane| {
                pane.tab_id == tab_id
                    && pane.phase == super::workspace_runtime::RuntimePanePhase::RestorePending
                    && pane.session_id.is_none()
            })
        {
            self.remember_split_focus_intent(tab_id, pane_id, window, cx);
        }
    }

    /// 只在捕获来源仍拥有同一 Ready session 且仍属于原工作区布局时接受确认。
    ///
    /// @author kongweiguang
    fn split_target_source_is_current(&self, request: &SplitTargetPickerRequest) -> bool {
        let runtime_source_is_current =
            self.workspace_runtime
                .pane(&request.pane_id)
                .is_some_and(|pane| {
                    request.matches_source(
                        &pane.tab_id,
                        &pane.persisted.id,
                        pane.session_id.as_ref(),
                        pane.phase == super::workspace_runtime::RuntimePanePhase::Ready,
                    )
                });
        let layout_still_contains_source =
            self.workspace_controller
                .as_ref()
                .is_some_and(|controller| {
                    controller.snapshot().terminal_tabs.iter().any(|tab| {
                        tab.id() == &request.tab_id
                            && tab
                                .terminal_layout()
                                .is_some_and(|layout| layout.contains_pane(&request.pane_id))
                    })
                });
        runtime_source_is_current
            && layout_still_contains_source
            && self.center_panel == super::CenterPanel::Terminal
    }

    /// Picker 关闭先恢复打开前焦点；之后新 pane 绑定成功才可在用户未改焦点时接管输入。
    ///
    /// @author kongweiguang
    pub(super) fn handle_split_target_picker_event(
        &mut self,
        event: SplitTargetPickerEvent,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        let SplitTargetPickerEvent::Confirm { request, target } = event;
        if !self.split_target_picker.read(cx).matches_request(&request) {
            return;
        }
        let return_focus = self
            .split_target_picker
            .update(cx, |picker, cx| picker.close(window, cx));
        if let Some(focus) = return_focus {
            window.focus(&focus, cx);
        }

        if !self.split_target_source_is_current(&request) {
            self.status = Some(crate::locale::text(
                "分屏来源已变化；没有创建终端，请重新发起操作。",
                "The split source changed. No terminal was created; start again from the intended pane.",
            ));
            cx.notify();
            return;
        }
        let Some(controller) = self.workspace_controller.as_mut() else {
            self.status = Some(crate::locale::text(
                "工作区尚未就绪，无法创建分屏。",
                "The workspace is not ready to create a split.",
            ));
            cx.notify();
            return;
        };
        let result = controller.begin_split_for_target(
            &request.tab_id,
            &request.pane_id,
            target.target,
            target.title,
            request.direction,
            request.before,
        );
        match result {
            Ok(
                crate::features::workspace_controller::WorkspaceControllerEffect::ResourceOperation(
                    operation,
                ),
            ) => self.dispatch_split_resource_operation(operation, window, cx),
            Ok(_) => {
                self.status = Some(crate::locale::text(
                    "未能生成分屏操作，请重试。",
                    "No split operation was produced. Please retry.",
                ));
                cx.notify();
            }
            Err(error) => {
                self.status = Some(error.public_message().to_owned().into());
                cx.notify();
            }
        }
    }

    /// 新 pane 的会话输入 handler 异步绑定；保留当前焦点快照，只有新 pane Ready 且用户未改焦点才转交。
    ///
    /// @author kongweiguang
    pub(super) fn remember_split_focus_intent(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        window: &Window,
        cx: &mut Context<RootView>,
    ) {
        self.pane_focus_intent_generation =
            self.pane_focus_intent_generation.wrapping_add(1).max(1);
        let restore_generation = self.workspace_restore_generations.get(&pane_id).copied();
        self.pending_pane_focus_intent = Some(PendingPaneFocusIntent {
            generation: self.pane_focus_intent_generation,
            tab_id,
            pane_id,
            session_id: None,
            restore_generation,
            expected_focus: window.focused(cx),
        });
    }
}

/// 把目标变体和稳定 ID 编成列表键，让键盘高亮不依赖可能重复的显示名称。
///
/// @author kongweiguang
fn target_option_key(target: &WorkspaceTargetRef) -> String {
    match target {
        WorkspaceTargetRef::Local { profile_id } => {
            format!("local:{}", profile_id.as_deref().unwrap_or("default"))
        }
        WorkspaceTargetRef::Ssh { host_id } => format!("ssh:{}", host_id.as_str()),
        WorkspaceTargetRef::Telnet { host_id } => format!("telnet:{}", host_id.as_str()),
        WorkspaceTargetRef::Serial { host_id } => format!("serial:{}", host_id.as_str()),
        WorkspaceTargetRef::DockerContainer {
            host_id,
            container_id,
            runtime,
            ..
        } => format!("container:{runtime:?}:{}:{container_id}", host_id.as_str()),
        WorkspaceTargetRef::Unavailable { raw, .. } => format!("unavailable:{raw}"),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/split_target_picker.rs"]
mod tests;
