//! 左栏容器请求只追踪显式主机和固定项，不借用后来活动的终端目标。
//!
//! @author kongweiguang

use std::time::Duration;

use gpui_kit::{AppContext as _, Context, Window};
use kerminal_application::preferences::ToolRailToolId;
use kerminal_domain::{
    identity::{HostId, PaneId, TabId},
    operations::ContainerTarget,
    sessions::{SessionOwner, SessionTarget},
};

use crate::features::operations::{ContainerPanelContext, SidebarContainerReveal};

use super::{
    PendingTerminalCreationOwner, RootView, TerminalTargetRequest,
    workspace_runtime::RuntimePanePhase,
};

/// 异步请求保留最初点击的对象，后续只能由同一 pane 和导航代次完成。
/// @author kongweiguang
#[derive(Clone)]
pub(super) struct SidebarContainerRequest {
    id: String,
    host_id: HostId,
    pin: Option<(String, ContainerTarget)>,
    navigation_generation: u64,
    owner: Option<PendingTerminalCreationOwner>,
    tool_opened: bool,
}

/// 面板的失败和取消通知立即回收尚未创建 pane 的意图；错误回投当前工作台，指纹确认不设用户决策超时。
/// @author kongweiguang
pub(super) fn sync_preflight(
    root: &mut RootView,
    panel: &crate::features::connections::ConnectionsPanel,
) -> bool {
    if root
        .pending_sidebar_container
        .as_ref()
        .is_some_and(|request| {
            request.owner.is_none()
                && (request.navigation_generation != root.surface_navigation_generation
                    || !panel.sidebar_container_intent_is_current(&request.id))
        })
    {
        if let Some(message) = root
            .pending_sidebar_container
            .as_ref()
            .filter(|request| request.navigation_generation == root.surface_navigation_generation)
            .and_then(|request| panel.sidebar_container_failure(&request.id))
        {
            root.status = Some(message.to_owned().into());
        }
        root.pending_sidebar_container = None;
        return true;
    }
    false
}

/// 从当前快照解析固定对象；不能按名字、选中项或另一个主机推断容器身份。
/// @author kongweiguang
fn pinned_target(root: &RootView, stable_id: &str) -> Option<ContainerTarget> {
    root.workspace_controller
        .as_ref()?
        .snapshot()
        .pinned_containers
        .iter()
        .find(|pin| pin.stable_id() == stable_id)
        .map(|pin| pin.target.clone())
}

/// 只有目录中仍为 SSH 的明确主机才可以进入容器或文件操作。
/// @author kongweiguang
fn host_is_available(root: &RootView, host_id: &HostId, cx: &Context<RootView>) -> bool {
    root.host_navigation
        .as_ref()
        .is_some_and(|navigation| navigation.read(cx).ssh_host_group_id(host_id).is_some())
}

/// 文件入口直接使用固定项的完整 host/runtime/container 端点，绝不回退到宿主 SFTP。
/// @author kongweiguang
pub(super) fn open_pinned_files(
    root: &mut RootView,
    stable_id: &str,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    root.pending_sidebar_container = None;
    let Some(target) = pinned_target(root, stable_id) else {
        root.status = Some(crate::locale::text(
            "固定容器已不存在。",
            "The pinned container no longer exists.",
        ));
        cx.notify();
        return;
    };
    if !target
        .host_id
        .as_ref()
        .is_some_and(|id| host_is_available(root, id, cx))
    {
        root.status = Some(crate::locale::text(
            "固定容器的 SSH 主机已不存在或协议已变化。",
            "The pinned container's SSH host no longer exists or its protocol changed.",
        ));
        cx.notify();
        return;
    }
    super::operations::open_container_files(root, &target, "/", window, cx);
}

/// 解析固定项后复用宿主请求，等待期间仍按完整固定项身份复核。
/// @author kongweiguang
pub(super) fn open_pinned_details(
    root: &mut RootView,
    stable_id: &str,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    let Some(target) = pinned_target(root, stable_id) else {
        root.status = Some(crate::locale::text(
            "固定容器已不存在。",
            "The pinned container no longer exists.",
        ));
        cx.notify();
        return;
    };
    let Some(host_id) = target.host_id.clone() else {
        root.status = Some(crate::locale::text(
            "固定容器没有 SSH 宿主。",
            "The pinned container has no SSH host.",
        ));
        cx.notify();
        return;
    };
    open_host(
        root,
        host_id,
        Some((stable_id.to_owned(), target)),
        window,
        cx,
    );
}

/// 优先复用当前或唯一同 host 的真实 SSH pane；无可用 pane 时经连接面板执行指纹预检。
/// @author kongweiguang
pub(super) fn open_host(
    root: &mut RootView,
    host_id: HostId,
    pin: Option<(String, ContainerTarget)>,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    root.pending_sidebar_container = None;
    if !host_is_available(root, &host_id, cx) {
        root.status = Some(crate::locale::text(
            "SSH 主机已不存在或协议已变化。",
            "The SSH host no longer exists or its protocol changed.",
        ));
        cx.notify();
        return;
    }
    let id = uuid::Uuid::new_v4().to_string();
    let mut request = SidebarContainerRequest {
        id: id.clone(),
        host_id: host_id.clone(),
        pin,
        navigation_generation: root.surface_navigation_generation,
        owner: None,
        tool_opened: false,
    };
    match existing_ssh_pane(root, &host_id) {
        ExistingSshPane::One { tab_id, pane_id } => {
            root.activate_workspace_tab(&tab_id, window, cx);
            root.focus_workspace_pane(tab_id.clone(), pane_id.clone(), window, cx);
            request.navigation_generation = root.surface_navigation_generation;
            request.owner = Some(PendingTerminalCreationOwner {
                generation: root
                    .workspace_restore_generations
                    .get(&pane_id)
                    .copied()
                    .unwrap_or(0),
                tab_id,
                pane_id,
            });
            root.pending_sidebar_container = Some(request);
            watch_request(root, id, cx);
        }
        ExistingSshPane::Multiple => {
            root.status = Some(crate::locale::text(
                "此主机有多个终端，请先在工作区选择要使用的终端。",
                "This host has multiple terminals. Select the intended terminal in the workspace first.",
            ));
            cx.notify();
        }
        ExistingSshPane::None => {
            let Some(panel) = root.connections_panel.clone() else {
                root.status = Some(crate::locale::text(
                    "主机连接服务尚未配置。",
                    "Host connection services are unavailable.",
                ));
                cx.notify();
                return;
            };
            root.pending_sidebar_container = Some(request);
            let started = panel.update(cx, |panel, cx| {
                panel.request_connect_host_for_container(&host_id, id, cx)
            });
            if !started {
                root.pending_sidebar_container = None;
                root.status = Some(crate::locale::text(
                    "主机目录已变化，请刷新后重试。",
                    "The host directory changed. Refresh and try again.",
                ));
                cx.notify();
            }
        }
    }
}

/// 将经过指纹预检的原请求绑定到实际新建 pane；过期回执不得打开终端或容器面板。
/// @author kongweiguang
pub(super) fn accept_connection(
    root: &mut RootView,
    intent_id: &str,
    host_id: &HostId,
    target: &SessionTarget,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    let Some(mut request) = root.pending_sidebar_container.take() else {
        return;
    };
    if request.id != intent_id {
        root.pending_sidebar_container = Some(request);
        return;
    }
    if !preflight_matches(
        &request,
        intent_id,
        host_id,
        target,
        root.surface_navigation_generation,
    ) || !request_pin_is_current(root, &request)
    {
        return;
    }
    if root.is_creating_session {
        root.status = Some(crate::locale::text(
            "正在创建其他终端，请稍后重试。",
            "Another terminal is being created. Try again when it finishes.",
        ));
        cx.notify();
        return;
    }
    root.create_terminal_for_target(
        TerminalTargetRequest::ExplicitHost {
            host_id: host_id.clone(),
            target: target.clone(),
        },
        window,
        cx,
    );
    let Some(owner) = root.pending_terminal_creation_owner.clone() else {
        return;
    };
    request.owner = Some(owner);
    request.navigation_generation = root.surface_navigation_generation;
    root.pending_sidebar_container = Some(request);
    watch_request(root, intent_id.to_owned(), cx);
}

/// 同主机的普通连接不是容器请求；精确 ID、导航代次和交互式 SSH 目标必须全部一致。
/// @author kongweiguang
fn preflight_matches(
    request: &SidebarContainerRequest,
    intent_id: &str,
    host_id: &HostId,
    target: &SessionTarget,
    navigation_generation: u64,
) -> bool {
    request.id == intent_id
        && request.navigation_generation == navigation_generation
        && &request.host_id == host_id
        && matches!(target, SessionTarget::Ssh(ssh) if &ssh.host_id == host_id && ssh.command.is_none())
}

/// 优先活动 pane，否则只在唯一候选时复用；多候选必须交由用户明确选定。
/// @author kongweiguang
fn existing_ssh_pane(root: &RootView, host_id: &HostId) -> ExistingSshPane {
    let Some(controller) = root.workspace_controller.as_ref() else {
        return ExistingSshPane::None;
    };
    let snapshot = controller.snapshot();
    let candidates = root.workspace_runtime.panes().filter_map(|(pane_id, pane)| {
        if pane.phase != RuntimePanePhase::Ready || pane.session_id.is_none()
            || !matches!(pane.target.as_ref(), Some(SessionTarget::Ssh(ssh)) if &ssh.host_id == host_id && ssh.command.is_none())
            || !snapshot.terminal_tabs.iter().any(|tab| tab.id() == &pane.tab_id && tab.terminal_layout().is_some_and(|layout| layout.contains_pane(pane_id)))
        { return None; }
        Some((pane.tab_id.clone(), pane_id.clone()))
    }).collect::<Vec<_>>();
    select_ssh_pane(
        candidates,
        snapshot.active_tab_id.as_ref(),
        snapshot.focused_pane_id.as_ref(),
    )
}

/// 多个 pane 时仅当前焦点具有明确意图，不能按列表顺序替用户选择。
/// @author kongweiguang
fn select_ssh_pane(
    candidates: Vec<(TabId, PaneId)>,
    active_tab: Option<&TabId>,
    focused_pane: Option<&PaneId>,
) -> ExistingSshPane {
    if let Some((tab_id, pane_id)) = candidates
        .iter()
        .find(|(tab, pane)| Some(tab) == active_tab && Some(pane) == focused_pane)
    {
        return ExistingSshPane::One {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        };
    }
    match candidates.as_slice() {
        [] => ExistingSshPane::None,
        [(tab_id, pane_id)] => ExistingSshPane::One {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        },
        _ => ExistingSshPane::Multiple,
    }
}

#[derive(Debug, Eq, PartialEq)]
enum ExistingSshPane {
    None,
    One { tab_id: TabId, pane_id: PaneId },
    Multiple,
}

/// 固定对象在等待期间被删除或替换时不能再打开其旧详情。
/// @author kongweiguang
fn request_pin_is_current(root: &RootView, request: &SidebarContainerRequest) -> bool {
    request
        .pin
        .as_ref()
        .is_none_or(|(id, target)| pinned_target(root, id).as_ref() == Some(target))
}

/// 有界等待 live binding 与容器列表；任何导航或请求替换都会停止续接，不抢回后续焦点。
/// @author kongweiguang
fn watch_request(root: &mut RootView, id: String, cx: &mut Context<RootView>) {
    let window_handle = root.window_handle;
    cx.spawn(async move |root, cx| {
        for _ in 0..300 {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            let result = cx.update_window(window_handle, |_, window, app| {
                root.update(app, |root, cx| advance_request(root, &id, window, cx))
            });
            if !matches!(result, Ok(Ok(true))) {
                return;
            }
        }
        let _ = root.update(cx, |root, cx| {
            if root
                .pending_sidebar_container
                .as_ref()
                .is_some_and(|pending| pending.id == id)
            {
                root.pending_sidebar_container = None;
                root.status = Some(crate::locale::text(
                    "容器目标未能及时确认，请重试。",
                    "The container target could not be confirmed in time. Try again.",
                ));
                cx.notify();
            }
        });
    })
    .detach();
}

/// 每次推进都复核 pane、session、窗口与 pin；只有验证通过的面板可以读取固定容器。
/// @author kongweiguang
fn advance_request(
    root: &mut RootView,
    id: &str,
    window: &mut Window,
    cx: &mut Context<RootView>,
) -> bool {
    let Some(request) = root.pending_sidebar_container.clone() else {
        return false;
    };
    if request.id != id {
        return false;
    }
    if request.navigation_generation != root.surface_navigation_generation
        || !request_pin_is_current(root, &request)
        || !host_is_available(root, &request.host_id, cx)
        || root.host_shutdown_requested
        || root.preferences_modal_open
        || root.connections_modal_open
        || root.connection_prompt_modal_open
        || window.has_active_prompt()
    {
        root.pending_sidebar_container = None;
        return false;
    }
    let Some(owner) = request.owner.as_ref() else {
        return false;
    };
    let Some(pane) = root.workspace_runtime.pane(&owner.pane_id) else {
        root.pending_sidebar_container = None;
        return false;
    };
    if root
        .workspace_restore_generations
        .get(&owner.pane_id)
        .copied()
        .unwrap_or(0)
        != owner.generation
    {
        root.pending_sidebar_container = None;
        return false;
    }
    if pane.phase == RuntimePanePhase::RestorePending {
        return true;
    }
    if pane.phase != RuntimePanePhase::Ready
        || pane.tab_id != owner.tab_id
        || !matches!(pane.target.as_ref(), Some(SessionTarget::Ssh(ssh)) if ssh.host_id == request.host_id && ssh.command.is_none())
    {
        root.pending_sidebar_container = None;
        return false;
    }
    let snapshot_matches = root
        .workspace_controller
        .as_ref()
        .is_some_and(|controller| {
            let snapshot = controller.snapshot();
            snapshot.active_tab_id.as_ref() == Some(&owner.tab_id)
                && snapshot.focused_pane_id.as_ref() == Some(&owner.pane_id)
        });
    if !snapshot_matches {
        root.pending_sidebar_container = None;
        return false;
    }
    let context = root.current_container_panel_context();
    let context_matches = matches!(&context, ContainerPanelContext::Host { binding, host_id: Some(host_id) }
        if host_id == &request.host_id && pane.session_id.as_ref() == Some(&binding.session_id)
        && binding.owner == (SessionOwner::Ui { tab_id: owner.tab_id.clone(), pane_id: owner.pane_id.clone() }));
    if !context_matches {
        return true;
    }
    if !request.tool_opened {
        show_container_tool(root, window, cx);
        if let Some(current) = root.pending_sidebar_container.as_mut() {
            current.tool_opened = true;
        }
        if request.pin.is_none() {
            root.pending_sidebar_container = None;
            return false;
        }
        return true;
    }
    if !container_tool_is_visible(root) {
        root.pending_sidebar_container = None;
        return false;
    }
    let Some((_, target)) = request.pin.as_ref() else {
        root.pending_sidebar_container = None;
        return false;
    };
    let Some(view) = root
        .tool_rail_operations_views
        .get(&ToolRailToolId::Containers)
        .cloned()
    else {
        return true;
    };
    match view.update(cx, |view, cx| {
        view.reveal_sidebar_container(&context, target, cx)
    }) {
        SidebarContainerReveal::Waiting => true,
        SidebarContainerReveal::Opened => {
            root.pending_sidebar_container = None;
            false
        }
        SidebarContainerReveal::Unavailable => {
            root.pending_sidebar_container = None;
            root.status = Some(crate::locale::text(
                "无法在当前主机的容器列表中确认固定容器，请刷新后重试。",
                "The pinned container could not be confirmed in the current host's container list. Refresh and try again.",
            ));
            cx.notify();
            false
        }
    }
}

/// 用户关闭或替换容器工具后结束等待，避免列表迟到时重新切回详情。
/// @author kongweiguang
fn container_tool_is_visible(root: &mut RootView) -> bool {
    if root.tool_rail_rendered_compact.0 {
        return root.compact_tool_drawer_open
            && root.tool_rail_rendered_compact.1 == Some(ToolRailToolId::Containers);
    }
    let settings = root.current_tool_rail_settings();
    let slots = root.tool_rail_state.project(&settings).panels;
    [slots.left, slots.bottom, slots.center].contains(&Some(ToolRailToolId::Containers))
        || (slots.attached == Some(ToolRailToolId::Containers) && !root.right_dock_collapsed)
}

/// 幂等显示现有工具，保留宽窄布局与用户设置决定的宿主位置。
/// @author kongweiguang
fn show_container_tool(root: &mut RootView, window: &mut Window, cx: &mut Context<RootView>) {
    if root.tool_rail_rendered_compact.0 {
        root.show_compact_tool_panel(ToolRailToolId::Containers, window, cx);
    } else {
        let settings = root.current_tool_rail_settings();
        let slots = root.tool_rail_state.project(&settings).panels;
        let visible = [slots.attached, slots.left, slots.bottom, slots.center]
            .contains(&Some(ToolRailToolId::Containers));
        if !visible
            || (slots.attached == Some(ToolRailToolId::Containers) && root.right_dock_collapsed)
        {
            root.toggle_tool_rail_panel(ToolRailToolId::Containers, window, cx);
        }
    }
    root.refresh_operations_panel_targets(cx);
}

#[cfg(test)]
#[path = "../../tests/unit/root_sidebar_containers.rs"]
mod tests;
