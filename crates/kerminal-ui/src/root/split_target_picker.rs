//! 分屏目标选择弹层；只负责搜索、显式选择与关闭，不启动连接或变更 workspace。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeSet, VecDeque},
    rc::Rc,
};

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
        HostPickerGroup, HostPickerList, HostPickerListMessage, HostPickerMultiSelect,
        HostPickerOption, HostPickerPopoverProjection, HostPickerPopoverRequest,
        HostPickerRowSelection, toggled_host_keys,
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
    /// 多选批量打开；Root 在每个会话 ACK 后串行启动下一个目标。
    ConfirmMany {
        request: SplitTargetPickerRequest,
        targets: Vec<WorkspaceSplitTargetOption>,
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
    /// 勾选按主机 target key 保存；分组行只展开为组内主机，打开时无需再区分“目录还是主机”。
    selected_keys: BTreeSet<String>,
    /// Shift 连选锚点使用稳定行 key；搜索隐藏锚点后由下一行重新建立。
    selection_anchor: Option<String>,
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
            selected_keys: BTreeSet::new(),
            selection_anchor: None,
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
        self.selected_keys.clear();
        self.selection_anchor = None;
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
        self.selected_keys.clear();
        self.selection_anchor = None;
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

    /// 勾选一行或整组：整组已选时取消，否则补齐；单个主机行因此就是标准勾选切换。
    ///
    /// 勾选集只保存主机 key，分组行不会被单独持久化，打开时自然展开为组内主机。
    ///
    /// @author kongweiguang
    fn toggle_selected_row(&mut self, row: HostPickerRowSelection, cx: &mut Context<Self>) {
        self.selected_keys = toggled_host_keys(&self.selected_keys, &row.host_keys);
        if row.adopt_anchor {
            self.selection_anchor = Some(row.row_key);
        }
        self.status = None;
        cx.notify();
    }

    /// Shift 连选只追加不取消，保持文件列表的多选直觉；锚点仍指向上一次主动勾选。
    ///
    /// @author kongweiguang
    fn extend_selection(&mut self, row: HostPickerRowSelection, cx: &mut Context<Self>) {
        self.selected_keys.extend(row.host_keys);
        if row.adopt_anchor {
            self.selection_anchor = Some(row.row_key);
        }
        self.status = None;
        cx.notify();
    }

    /// 按当前目录重核已勾选目标，配置重载后消失的主机不会进入批量启动。
    ///
    /// @author kongweiguang
    fn selected_options(&mut self, cx: &Context<Self>) -> Vec<WorkspaceSplitTargetOption> {
        let options = self.options(cx);
        let current_keys = options
            .iter()
            .map(|option| target_option_key(&option.target))
            .collect::<BTreeSet<_>>();
        self.selected_keys.retain(|key| current_keys.contains(key));
        options
            .into_iter()
            .filter(|option| {
                self.selected_keys
                    .contains(&target_option_key(&option.target))
            })
            .collect()
    }

    /// 一次打开全部已勾选目标；单个选择仍走既有单次确认，批量由 Root 串行创建。
    ///
    /// @author kongweiguang
    fn open_selected_targets(&mut self, cx: &mut Context<Self>) {
        let Some(request) = self.request.clone() else {
            return;
        };
        let targets = self.selected_options(cx);
        match targets.len() {
            0 => {
                self.status = Some(crate::locale::text(
                    "所选目标已不在当前目录，请重新勾选。",
                    "The selected targets are no longer in the current directory; select them again.",
                ));
                cx.notify();
            }
            1 => cx.emit(SplitTargetPickerEvent::Confirm {
                request,
                target: targets[0].clone(),
            }),
            _ => cx.emit(SplitTargetPickerEvent::ConfirmMany { request, targets }),
        }
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

    /// 把勾选集、锚点和三个动作打包给通用列表；列表不反向持有选择器业务状态。
    ///
    /// @author kongweiguang
    fn multi_select_projection(&self, cx: &Context<Self>) -> HostPickerMultiSelect {
        let toggle_picker = cx.entity();
        let range_picker = cx.entity();
        let open_picker = cx.entity();
        HostPickerMultiSelect {
            selected_keys: self.selected_keys.clone(),
            anchor: self.selection_anchor.clone(),
            on_toggle: Rc::new(move |row, _window, app| {
                toggle_picker.update(app, |picker, cx| picker.toggle_selected_row(row, cx));
            }),
            on_range: Rc::new(move |row, _window, app| {
                range_picker.update(app, |picker, cx| picker.extend_selection(row, cx));
            }),
            on_open_selected: Rc::new(move |_window, app| {
                open_picker.update(app, |picker, cx| picker.open_selected_targets(cx));
            }),
        }
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
        .multi_select(self.multi_select_projection(cx))
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
    /// 多选确认沿用同一来源校验，批量由 Root 串行启动，选择器本身不持有资源生命周期。
    ///
    /// @author kongweiguang
    pub(super) fn handle_split_target_picker_event(
        &mut self,
        event: SplitTargetPickerEvent,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        let (request, mut targets) = match event {
            SplitTargetPickerEvent::Confirm { request, target } => (request, vec![target]),
            SplitTargetPickerEvent::ConfirmMany { request, targets } => (request, targets),
        };
        if !self.split_target_picker.read(cx).matches_request(&request) {
            return;
        }
        let return_focus = self
            .split_target_picker
            .update(cx, |picker, cx| picker.close(window, cx));
        if let Some(focus) = return_focus {
            window.focus(&focus, cx);
        }

        if targets.is_empty() {
            return;
        }
        if !self.split_target_source_is_current(&request) {
            self.status = Some(crate::locale::text(
                "分屏来源已变化；没有创建终端，请重新发起操作。",
                "The split source changed. No terminal was created; start again from the intended pane.",
            ));
            cx.notify();
            return;
        }
        if targets.len() == 1 {
            if let Some(target) = targets.pop() {
                self.start_split_target(request, target, window, cx);
            }
            return;
        }
        self.start_split_target_batch(request, targets, window, cx);
    }

    /// 单击分屏保持既有单一 CreatePane 路径，不与批量共享额外的队列状态。
    ///
    /// @author kongweiguang
    fn start_split_target(
        &mut self,
        request: SplitTargetPickerRequest,
        target: WorkspaceSplitTargetOption,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
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

    /// 多选分屏先启动首个目标，其余按会话 ACK 顺序排队，避免 controller 并发候选互相覆盖。
    ///
    /// @author kongweiguang
    fn start_split_target_batch(
        &mut self,
        request: SplitTargetPickerRequest,
        mut targets: Vec<WorkspaceSplitTargetOption>,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        let Some(first) = targets.first().cloned() else {
            return;
        };
        let remaining = targets
            .drain(1..)
            .map(|target| (target.target, target.title))
            .collect::<VecDeque<_>>();
        let total = remaining.len() + 1;
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
            first.target,
            first.title,
            request.direction,
            request.before,
        );
        match result {
            Ok(
                crate::features::workspace_controller::WorkspaceControllerEffect::ResourceOperation(
                    operation,
                ),
            ) => {
                let Some(pane_id) = created_split_pane_id(&operation) else {
                    self.status = Some(crate::locale::text(
                        "未能确认分屏操作身份，已停止批量打开。",
                        "The split operation identity could not be confirmed; batch opening stopped.",
                    ));
                    cx.notify();
                    return;
                };
                let label = split_batch_progress_label(1, total);
                // 新批次开始时不能沿用上一次中止摘要，否则失败候选丢弃时会误清当前提示。
                self.split_batch_abort_status = None;
                self.split_open_batch = Some(PendingSplitBatch {
                    tab_id: request.tab_id,
                    source_pane_id: request.pane_id,
                    direction: request.direction,
                    before: request.before,
                    total,
                    opened: 0,
                    current_pane: Some(pane_id),
                    last_added_pane: None,
                    remaining,
                    last_status: Some(label.clone()),
                });
                self.status = Some(label);
                self.dispatch_split_resource_operation(operation, window, cx);
            }
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

    /// 只有当前批次的候选会话真正 Ready 后才推进下一个；迟到或重复 ACK 不重复启动。
    ///
    /// @author kongweiguang
    pub(super) fn note_split_batch_pane_opened(
        &mut self,
        pane_id: &PaneId,
        cx: &mut Context<RootView>,
    ) {
        let (finished, last_status) = {
            let Some(batch) = self.split_open_batch.as_mut() else {
                return;
            };
            if batch.current_pane.as_ref() != Some(pane_id) {
                return;
            }
            batch.current_pane = None;
            batch.opened += 1;
            batch.last_added_pane = Some(pane_id.clone());
            (batch.remaining.is_empty(), batch.last_status.clone())
        };
        if finished {
            self.split_open_batch = None;
            if self.status.is_some() && self.status == last_status {
                self.status = None;
                cx.notify();
            }
            return;
        }
        self.schedule_split_batch_continuation(cx);
    }

    /// 续跑放到下一次窗口更新，复用异步 ACK 回调既有的窗口上下文与 Dialog 门禁。
    ///
    /// @author kongweiguang
    fn schedule_split_batch_continuation(&mut self, cx: &mut Context<RootView>) {
        let window_handle = self.window_handle;
        cx.spawn(async move |this, cx| {
            let _ = cx.update_window(window_handle, move |_, window, app| {
                let _ = this.update(app, |root, cx| root.continue_split_batch(window, cx));
            });
        })
        .detach();
    }

    /// 批次续跑前重新核对来源、插入位置与 controller 空闲；任一失效都收尾并告知已打开数量。
    ///
    /// @author kongweiguang
    pub(super) fn continue_split_batch(&mut self, window: &mut Window, cx: &mut Context<RootView>) {
        let Some((tab_id, source_pane_id, direction, before, insert_neighbor, next_index, total)) =
            self.split_open_batch.as_ref().and_then(|batch| {
                if batch.remaining.is_empty() || batch.current_pane.is_some() {
                    return None;
                }
                let insert_neighbor = if batch.before {
                    batch.source_pane_id.clone()
                } else {
                    batch
                        .last_added_pane
                        .clone()
                        .unwrap_or_else(|| batch.source_pane_id.clone())
                };
                Some((
                    batch.tab_id.clone(),
                    batch.source_pane_id.clone(),
                    batch.direction,
                    batch.before,
                    insert_neighbor,
                    batch.opened + 1,
                    batch.total,
                ))
            })
        else {
            return;
        };
        let source_is_current = self
            .workspace_runtime
            .pane(&source_pane_id)
            .is_some_and(|pane| {
                pane.tab_id == tab_id
                    && pane.phase == super::workspace_runtime::RuntimePanePhase::Ready
                    && pane.session_id.is_some()
            })
            && self
                .workspace_controller
                .as_ref()
                .is_some_and(|controller| {
                    controller.snapshot().terminal_tabs.iter().any(|tab| {
                        tab.id() == &tab_id
                            && tab.terminal_layout().is_some_and(|layout| {
                                layout.contains_pane(&source_pane_id)
                                    && layout.contains_pane(&insert_neighbor)
                            })
                    })
                });
        if !source_is_current {
            self.abort_split_batch(
                Some(crate::locale::text(
                    "分屏来源或插入位置已变化。",
                    "The split source or insertion position changed.",
                )),
                cx,
            );
            return;
        }
        let Some((target, title)) = self
            .split_open_batch
            .as_mut()
            .and_then(|batch| batch.remaining.pop_front())
        else {
            return;
        };
        let Some(controller) = self.workspace_controller.as_mut() else {
            self.abort_split_batch(
                Some(crate::locale::text(
                    "工作区尚未就绪。",
                    "The workspace is not ready.",
                )),
                cx,
            );
            return;
        };
        let result = controller.append_split_for_target(
            &tab_id,
            &source_pane_id,
            crate::features::workspace_controller::WorkspaceSplitInsertion {
                neighbor_pane_id: insert_neighbor,
                target,
                title,
                direction,
                before,
            },
        );
        match result {
            Ok(
                crate::features::workspace_controller::WorkspaceControllerEffect::ResourceOperation(
                    operation,
                ),
            ) => {
                let Some(pane_id) = created_split_pane_id(&operation) else {
                    self.abort_split_batch(
                        Some(crate::locale::text(
                            "未能确认分屏操作身份。",
                            "The split operation identity could not be confirmed.",
                        )),
                        cx,
                    );
                    return;
                };
                let label = split_batch_progress_label(next_index, total);
                if let Some(batch) = self.split_open_batch.as_mut() {
                    batch.current_pane = Some(pane_id);
                    batch.last_status = Some(label.clone());
                }
                self.status = Some(label);
                self.dispatch_split_resource_operation(operation, window, cx);
            }
            Ok(_) => {
                self.abort_split_batch(
                    Some(crate::locale::text(
                        "未能生成分屏操作。",
                        "No split operation was produced.",
                    )),
                    cx,
                );
            }
            Err(error) => {
                self.abort_split_batch(Some(SharedString::from(error.public_message())), cx);
            }
        }
    }

    /// 当前候选失败时收起整批；已打开数量和失败原因一次告知，剩余目标不再自动启动。
    ///
    /// @author kongweiguang
    pub(super) fn abort_split_batch_for_pane(
        &mut self,
        pane_id: &PaneId,
        reason: Option<SharedString>,
        cx: &mut Context<RootView>,
    ) -> bool {
        let matches = self
            .split_open_batch
            .as_ref()
            .is_some_and(|batch| batch.current_pane.as_ref() == Some(pane_id));
        if !matches {
            return false;
        }
        self.abort_split_batch(reason, cx);
        true
    }

    /// 用户主动取消或关闭当前分屏时静默收起批次，只保留用户动作自己的结果文案。
    ///
    /// @author kongweiguang
    pub(super) fn discard_split_batch_for_pane(
        &mut self,
        pane_id: &PaneId,
        cx: &mut Context<RootView>,
    ) {
        let matches = self.split_open_batch.as_ref().is_some_and(|batch| {
            batch.current_pane.as_ref() == Some(pane_id) || batch.source_pane_id == *pane_id
        });
        if !matches {
            return;
        }
        if let Some(batch) = self.split_open_batch.take()
            && self.status.is_some()
            && self.status == batch.last_status
        {
            self.status = None;
        }
        cx.notify();
    }

    /// 失败收尾把“已打开/总数/剩余取消”合并成一条可见状态，避免残留进行中文案。
    ///
    /// @author kongweiguang
    fn abort_split_batch(
        &mut self,
        reason: Option<SharedString>,
        cx: &mut Context<RootView>,
    ) -> bool {
        let Some(batch) = self.split_open_batch.take() else {
            return false;
        };
        let cancelled = usize::from(batch.current_pane.is_some()) + batch.remaining.len();
        let label =
            split_batch_failure_label(batch.opened, batch.total, cancelled, reason.as_deref());
        // 只记录本次中止摘要；自动丢弃失败候选时按同一字符串精确清理，
        // 不再依赖易漂移的文案前缀判断。
        self.split_batch_abort_status = Some(label.clone());
        self.status = Some(label);
        self.refresh_workspace_views(cx);
        cx.notify();
        true
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

/// 多选分屏批次：一次只占用 controller 的单个候选资源槽位，每个 ACK 后串行启动下一个。
///
/// @author kongweiguang
pub(super) struct PendingSplitBatch {
    pub(super) tab_id: TabId,
    pub(super) source_pane_id: PaneId,
    pub(super) direction: kerminal_domain::workspace::SplitDirection,
    pub(super) before: bool,
    pub(super) total: usize,
    pub(super) opened: usize,
    /// 当前已派发但尚未 ACK 的候选；旧回执或迟到取消按此精确匹配。
    pub(super) current_pane: Option<PaneId>,
    /// before=false 时紧邻上次新 pane 追加，保持选择顺序；before=true 始终贴着来源。
    pub(super) last_added_pane: Option<PaneId>,
    pub(super) remaining: VecDeque<(WorkspaceTargetRef, String)>,
    pub(super) last_status: Option<SharedString>,
}

/// 只接受 CreatePane 作为批次候选，避免把其它资源操作误塞进队列。
///
/// @author kongweiguang
fn created_split_pane_id(
    operation: &crate::features::workspace_controller::WorkspaceResourceOperation,
) -> Option<PaneId> {
    match operation {
        crate::features::workspace_controller::WorkspaceResourceOperation::CreatePane {
            pane,
            ..
        } => Some(pane.id.clone()),
        crate::features::workspace_controller::WorkspaceResourceOperation::ClosePane { .. }
        | crate::features::workspace_controller::WorkspaceResourceOperation::CloseTab { .. } => {
            None
        }
    }
}

/// 进度文案只报告真实 ACK 序号，避免把排队目标提前算成已打开。
///
/// @author kongweiguang
fn split_batch_progress_label(next: usize, total: usize) -> SharedString {
    match crate::locale::current_language() {
        crate::locale::UiLanguage::ZhCn => {
            SharedString::from(format!("正在打开分屏 {next}/{total}…"))
        }
        crate::locale::UiLanguage::EnUs => {
            SharedString::from(format!("Opening split {next}/{total}…"))
        }
    }
}

/// 失败摘要保留已打开数量和取消数量，让用户知道哪些目标没有启动。
///
/// @author kongweiguang
fn split_batch_failure_label(
    opened: usize,
    total: usize,
    cancelled: usize,
    reason: Option<&str>,
) -> SharedString {
    let base = match crate::locale::current_language() {
        crate::locale::UiLanguage::ZhCn => {
            if opened == 0 {
                "未能打开分屏。".to_owned()
            } else if cancelled > 0 {
                format!("已打开 {opened}/{total} 个分屏；其余目标已取消。")
            } else {
                format!("已打开 {opened}/{total} 个分屏。")
            }
        }
        crate::locale::UiLanguage::EnUs => {
            if opened == 0 {
                "No splits were opened.".to_owned()
            } else if cancelled > 0 {
                format!("Opened {opened} of {total} splits; the remaining targets were cancelled.")
            } else {
                format!("Opened {opened} of {total} splits.")
            }
        }
    };
    match reason {
        Some(reason) if !reason.trim().is_empty() => SharedString::from(format!("{base} {reason}")),
        _ => SharedString::from(base),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/split_target_picker.rs"]
mod tests;
