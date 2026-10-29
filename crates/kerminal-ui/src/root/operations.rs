//! Operations 面板到 Root 工作区/文件面的窄事件接缝。
//!
//! @author kongweiguang

use std::ffi::OsStr;

use gpui_kit::{Context, Entity, Window};
use kerminal_application::sessions::UiSessionBinding;
use kerminal_application::workspace::{
    WorkspaceContainerRuntime, WorkspaceLocalMachineScope, WorkspaceMachineStatus,
    WorkspacePaneMode, WorkspaceSessionPane, WorkspaceTargetRef, WorkspaceTmuxBinding,
};
use kerminal_domain::{
    connections::RemoteShellDialect,
    files::{ContainerRuntime, FileEndpoint, FilePathError, RemotePath},
    operations::{ContainerTarget, TmuxTarget},
    sessions::{SessionOwner, SessionTarget, TerminalSize},
};
use kerminal_terminal::input::TerminalKeyEvent;

use super::{
    CenterPanel, InputModifiers, KeyPhase, RootView, SessionCommand, TerminalInputEvent,
    TerminalKey, UiInputProjection, UiTerminalInputRequest, WorkspaceTerminalOpenKind,
    workspace_runtime::RuntimePanePhase,
};
use crate::features::operations::{
    ContainerPanelContext, OperationsEvent, OperationsView, TmuxAttachTarget,
    TmuxAttachTerminalBinding, TmuxQuickrefKind, TmuxQuickrefTerminalBinding,
};

/// 携带 Operations 点击时捕获的完整 tmux 目标，避免发送前重读当前选择。
///
/// @author kongweiguang
struct TmuxQuickrefRequest<'a> {
    pane_id: &'a kerminal_domain::identity::PaneId,
    binding: &'a UiSessionBinding,
    target: &'a TmuxTarget,
    session_id: &'a str,
    text: &'a str,
    kind: TmuxQuickrefKind,
}

/// 已验证的工作区终端意图；保持 pane 元数据和真正打开目标在同一处构造。
struct WorkspaceTerminalIntent {
    pane: WorkspaceSessionPane,
    title: String,
    kind: WorkspaceTerminalOpenKind,
    transient: bool,
}

/// 容器请求在提交服务前重验真实活动终端；确认与回执仍归原 Entity，会话创建沿用统一 owner。
/// @author kongweiguang
pub(super) fn handle_operations_event(
    root: &mut RootView,
    event: &OperationsEvent,
    operations_view: &Entity<crate::features::operations::OperationsView>,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    match event {
        OperationsEvent::RootConfirmationChanged => {}
        OperationsEvent::ContainerRefreshRequested { context, runtime } => {
            if container_context_is_current(root, operations_view, Some(context), true, cx) {
                operations_view.update(cx, |view, cx| {
                    view.submit_container_refresh_for_context(context, *runtime, cx);
                });
            }
        }
        OperationsEvent::ContainerMutationRequested { context, request } => {
            if container_context_is_current(root, operations_view, Some(context), true, cx) {
                operations_view.update(cx, |view, cx| {
                    view.submit_container_mutation_for_context(context, request.clone(), cx);
                });
            }
        }
        OperationsEvent::ComposeMutationRequested {
            context,
            target,
            action,
        } => {
            if container_context_is_current(root, operations_view, Some(context), true, cx) {
                operations_view.update(cx, |view, cx| {
                    view.submit_compose_mutation_for_context(context, target.clone(), *action, cx);
                });
            }
        }
        OperationsEvent::ContainerReadRequested {
            context,
            target,
            kind,
        } => {
            if container_context_is_current(root, operations_view, Some(context), false, cx) {
                operations_view.update(cx, |view, cx| {
                    view.submit_container_read_for_context(context, target.clone(), *kind, cx);
                });
            }
        }
        OperationsEvent::OpenContainerFilesRequested {
            target,
            path,
            container_panel_context,
        } => {
            if container_context_is_current(
                root,
                operations_view,
                container_panel_context.as_ref(),
                true,
                cx,
            ) {
                open_container_files(root, target, path, window, cx);
            }
        }
        OperationsEvent::OpenComposeConfigRequested {
            endpoint,
            selection_name,
            root_path,
            container_panel_context,
        } => {
            if container_context_is_current(
                root,
                operations_view,
                container_panel_context.as_ref(),
                true,
                cx,
            ) {
                open_compose_config(
                    root,
                    endpoint.clone(),
                    selection_name,
                    root_path.as_deref(),
                    window,
                    cx,
                );
            }
        }
        OperationsEvent::ComposeYamlPreviewRequested {
            context,
            request_id,
            endpoint,
        } => {
            if container_context_is_current(root, operations_view, context.as_ref(), true, cx) {
                read_compose_yaml_preview(
                    root,
                    operations_view,
                    *request_id,
                    endpoint.clone(),
                    window,
                    cx,
                );
            }
        }
        OperationsEvent::SendTmuxQuickrefRequested {
            pane_id,
            binding,
            target,
            session_id,
            text,
            kind,
        } => send_tmux_quickref(
            root,
            TmuxQuickrefRequest {
                pane_id,
                binding,
                target,
                session_id,
                text,
                kind: *kind,
            },
            cx,
        ),
        OperationsEvent::ContainerExecRequested {
            target,
            command,
            container_panel_context,
        } => {
            if container_context_is_current(
                root,
                operations_view,
                container_panel_context.as_ref(),
                true,
                cx,
            ) {
                open_operation_terminal(root, container_exec_intent(target, command), window, cx);
            }
        }
        OperationsEvent::ContainerLogsRequested {
            container_panel_context,
            target,
            name,
        } => {
            if container_context_is_current(
                root,
                operations_view,
                container_panel_context.as_ref(),
                true,
                cx,
            ) {
                open_operation_terminal(root, container_logs_intent(target, name), window, cx);
            }
        }
        OperationsEvent::ContainerPinRequested {
            context,
            target,
            container,
        } => {
            if container_context_is_current(root, operations_view, context.as_ref(), true, cx) {
                super::connections::pin_container_from_operations(
                    root,
                    context.as_ref(),
                    target,
                    container,
                    cx,
                );
            }
        }
        OperationsEvent::TmuxAttachRequested {
            target,
            session_id,
            session_name,
            terminal,
        } => {
            if let Some(binding) = attach_tmux_in_current_terminal(
                root,
                target,
                session_id,
                session_name,
                terminal.as_ref(),
                operations_view,
                cx,
            ) && let SessionOwner::Ui { tab_id, pane_id } = &binding.owner
            {
                // 成功提交后才交还原 pane 的原生文本焦点，失败不抢走用户正在编辑的表单。
                root.focused_agent_terminal = false;
                root.request_workspace_pane_focus(tab_id.clone(), pane_id.clone(), window, cx);
            }
        }
        OperationsEvent::TmuxDetachCurrentRequested { target, terminal } => {
            detach_tmux_in_current_terminal(root, target, terminal, operations_view, window, cx);
        }
    }
}

/// 同时核对冻结意图、来源视图和 Root live 目标，避免切 Tab 后未重绘的旧按钮仍能提交。
/// 校验失败只重投影视图，不替用户把旧请求改投当前主机；传统运维事件不能借空 context 绕过新面板。
///
/// @author kongweiguang
fn container_context_is_current(
    root: &mut RootView,
    operations_view: &Entity<OperationsView>,
    expected: Option<&ContainerPanelContext>,
    require_host: bool,
    cx: &mut Context<RootView>,
) -> bool {
    let source_view = operations_view.read(cx);
    let source = source_view.container_panel_context().cloned();
    let active = source_view.container_panel_is_active();
    if source.is_none() && expected.is_none() {
        return true;
    }
    let fresh = root.current_container_panel_context();
    let allowed =
        active && container_context_allows_request(source.as_ref(), expected, &fresh, require_host);
    operations_view.update(cx, |view, cx| {
        view.set_container_panel_context(fresh, cx);
    });
    if !allowed {
        root.status = Some(crate::locale::text(
            "当前终端目标已变化或不支持此操作，请在当前目标重试。",
            "The current terminal target changed or cannot perform this action. Retry on the current target.",
        ));
        cx.notify();
    }
    allowed
}

/// 写请求比较来源、冻结意图和 Root 当前上下文；WorkspaceHost 的 tab/host 身份必须逐项一致。
///
/// @author kongweiguang
fn container_context_allows_request(
    source: Option<&ContainerPanelContext>,
    expected: Option<&ContainerPanelContext>,
    fresh: &ContainerPanelContext,
    require_host: bool,
) -> bool {
    expected.is_some_and(|expected| {
        source == Some(expected)
            && expected == fresh
            && matches!(
                fresh,
                ContainerPanelContext::Host { .. }
                    | ContainerPanelContext::WorkspaceHost { .. }
                    | ContainerPanelContext::ContainerInfo { .. }
            )
            && (!require_host
                || matches!(
                    fresh,
                    ContainerPanelContext::Host { .. }
                        | ContainerPanelContext::WorkspaceHost { .. }
                ))
    })
}

/// Compose 文件只允许进入本机或 SSH 文件浏览器；容器 endpoint 属于不同文件系统，必须拒绝。
///
/// @author kongweiguang
fn read_compose_yaml_preview(
    root: &RootView,
    source: &Entity<OperationsView>,
    request_id: u64,
    endpoint: FileEndpoint,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    let failure = if !is_host_file_endpoint(&endpoint) {
        Some("Compose 配置必须来自本机或 SSH 主机文件。")
    } else {
        match (source.read(cx).container_panel_context(), &endpoint) {
            (
                Some(ContainerPanelContext::Host { host_id: None, .. }),
                FileEndpoint::Local { .. },
            )
            | (None, _) => None,
            (
                Some(ContainerPanelContext::Host {
                    host_id: Some(expected),
                    ..
                }),
                FileEndpoint::Remote { host_id, .. },
            ) if expected == host_id => None,
            (
                Some(ContainerPanelContext::WorkspaceHost {
                    host_id: expected, ..
                }),
                FileEndpoint::Remote { host_id, .. },
            ) if expected == host_id => None,
            _ => Some("Compose 配置目标已变化，请重新选择项目。"),
        }
    };
    if let Some(message) = failure {
        source.update(cx, |view, cx| {
            view.report_compose_yaml_preview(
                request_id,
                endpoint,
                Err(message.to_owned()),
                window,
                cx,
            );
        });
        return;
    }
    let Some(files) = root.services.files.clone() else {
        source.update(cx, |view, cx| {
            view.report_compose_yaml_preview(
                request_id,
                endpoint,
                Err("文件与传输服务尚未装配。".to_owned()),
                window,
                cx,
            );
        });
        return;
    };
    let executor = root.services.executor.clone();
    let source = source.downgrade();
    let window_handle = window.window_handle();
    let read_endpoint = endpoint.clone();
    cx.spawn(async move |_, cx| {
        let result = executor
            .run(move |cancellation| async move {
                files
                    .preview_file(
                        cancellation,
                        read_endpoint,
                        crate::features::operations::COMPOSE_YAML_PREVIEW_MAX_BYTES,
                    )
                    .await
            })
            .await
            .map_err(|error| error.public_message().to_owned());
        let _ = window_handle.update(cx, |_, window, app| {
            let _ = source.update(app, |view, cx| {
                view.report_compose_yaml_preview(request_id, endpoint, result, window, cx)
            });
        });
    })
    .detach();
}

/// Compose YAML 与 main 一样打开中央只读文件 Tab；保留冻结端点，不转向共享 SFTP 浏览器。
/// 描述登记与工作区快照提交必须同时成功，失败时撤销本轮登记，已存在的同源标签直接激活。
///
/// @author kongweiguang
fn open_compose_config(
    root: &mut RootView,
    endpoint: FileEndpoint,
    selection_name: &str,
    configured_root_path: Option<&str>,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    if !is_host_file_endpoint(&endpoint) {
        root.status = Some(crate::locale::text(
            "Compose 配置必须来自本机或 SSH 主机文件。",
            "Compose configuration must come from a local or SSH host file.",
        ));
        cx.notify();
        return;
    }
    if root.file_editor_workspace.is_none() {
        root.status = Some(crate::locale::text(
            "文件编辑服务尚未配置。",
            "The file editor service is unavailable.",
        ));
        cx.notify();
        return;
    }
    let (target, path, machine_id, root_path) = match &endpoint {
        FileEndpoint::Remote { host_id, path } => (
            WorkspaceTargetRef::Ssh {
                host_id: host_id.clone(),
            },
            path.to_string(),
            host_id.as_str().to_owned(),
            path.parent().map(|parent| parent.to_string()),
        ),
        FileEndpoint::Local { path } => (
            WorkspaceTargetRef::Local { profile_id: None },
            path.to_string_lossy().into_owned(),
            "local".to_owned(),
            path.parent()
                .map(|parent| parent.to_string_lossy().into_owned()),
        ),
        FileEndpoint::Container { .. } => return,
    };
    let root_path = configured_root_path.map(str::to_owned).or(root_path);
    let Some(controller) = root.workspace_controller.as_ref() else {
        root.status = Some("工作区尚未就绪，未打开 Compose YAML。".into());
        cx.notify();
        return;
    };
    use kerminal_application::workspace::{
        WorkspaceFileAccess, WorkspaceFileSource, WorkspaceSessionTab,
    };
    if let Some(id) = controller
        .snapshot()
        .terminal_tabs
        .iter()
        .find_map(|tab| match tab {
            WorkspaceSessionTab::WorkspaceFile {
                id,
                target: candidate,
                path: candidate_path,
                source: WorkspaceFileSource::ComposeYaml,
                access: WorkspaceFileAccess::Readonly,
                ..
            } if candidate == &target && candidate_path == &path => Some(id.clone()),
            _ => None,
        })
    {
        // 去重后仍须先提交 controller 的活动身份；只投影编辑器会在下一帧被原 Tab 覆盖。
        let event = crate::features::workspace_tabs::WorkspaceTabEvent::Activate(id.clone());
        if root.handle_workspace_tab_event(&event, window, cx) {
            root.activate_workspace_tab(&id, window, cx);
        }
        return;
    }
    let mut snapshot = controller.snapshot().clone();
    let id = kerminal_domain::identity::TabId::new();
    let tab = WorkspaceSessionTab::WorkspaceFile {
        id: id.clone(),
        tab_group_id: None,
        title: selection_name.to_owned(),
        machine_id,
        target,
        path,
        root_path,
        access: WorkspaceFileAccess::Readonly,
        source: WorkspaceFileSource::ComposeYaml,
    };
    snapshot.terminal_tabs.push(tab.clone());
    snapshot.active_tab_id = Some(id.clone());
    snapshot.focused_pane_id = None;
    if !root.workspace_files.register_pending_compose_yaml_tab(tab) {
        root.status = Some("文件工作区尚未就绪，未打开 Compose YAML。".into());
        cx.notify();
        return;
    }
    if let Err(error) = root
        .workspace_controller
        .as_mut()
        .expect("controller was checked above")
        .synchronize_snapshot(snapshot)
    {
        root.remove_workspace_file_tab_after_close(&id);
        root.status = Some(error.public_message().to_owned().into());
        cx.notify();
        return;
    }
    root.refresh_workspace_views(cx);
    root.activate_workspace_tab(&id, window, cx);
    root.persist_workspace_snapshot(cx);
}

/// Compose 只接受宿主文件系统端点，避免把 host 路径错送到容器文件浏览器。
///
/// @author kongweiguang
fn is_host_file_endpoint(endpoint: &FileEndpoint) -> bool {
    matches!(
        endpoint,
        FileEndpoint::Local { .. } | FileEndpoint::Remote { .. }
    )
}

/// 将 tmux 快捷内容转成精确 pane binding 的 UI 输入事件，不绕过 session FIFO 或终端协议编码。
///
/// @author kongweiguang
fn send_tmux_quickref(
    root: &mut RootView,
    request: TmuxQuickrefRequest<'_>,
    cx: &mut Context<RootView>,
) {
    let TmuxQuickrefRequest {
        pane_id,
        binding,
        target,
        session_id,
        text,
        kind,
    } = request;
    if !tmux_quickref_is_current(root, pane_id, binding, target, session_id) {
        root.status = Some(crate::locale::text(
            "tmux 终端目标已变化；快捷内容未发送，请重新选择。",
            "The tmux terminal target changed; nothing was sent. Select it again.",
        ));
        cx.notify();
        return;
    }
    if target.validate().is_err() || !valid_quickref_text(text, kind) {
        root.status = Some(crate::locale::text(
            "tmux 快捷内容无效，未发送。",
            "The tmux quickref payload is invalid and was not sent.",
        ));
        cx.notify();
        return;
    }

    let first_event = match kind {
        TmuxQuickrefKind::Command => TerminalInputEvent::Paste(text.to_owned()),
        TmuxQuickrefKind::Shortcut => TerminalInputEvent::Key(TerminalKeyEvent {
            key: TerminalKey::Character('b'),
            phase: KeyPhase::Press,
            modifiers: InputModifiers {
                control: true,
                ..InputModifiers::default()
            },
        }),
    };
    if !enqueue_tmux_event(root, binding, first_event, cx) {
        root.status = Some(crate::locale::text(
            "终端输入队列已拒绝此 Quickref，内容未发送。",
            "The terminal input queue rejected this quickref; nothing was sent.",
        ));
        cx.notify();
        return;
    }

    match kind {
        TmuxQuickrefKind::Command => {
            if !enqueue_tmux_event(
                root,
                binding,
                TerminalInputEvent::Key(TerminalKeyEvent {
                    key: TerminalKey::Enter,
                    phase: KeyPhase::Press,
                    modifiers: InputModifiers::default(),
                }),
                cx,
            ) {
                root.status = Some(crate::locale::text(
                    "命令已粘贴，但提交键未能排队；请检查终端后再操作。",
                    "The command was pasted, but Enter could not be queued. Check the terminal before continuing.",
                ));
                cx.notify();
            }
        }
        TmuxQuickrefKind::Shortcut => {
            let Some(key) = text.chars().nth(1) else {
                return;
            };
            if !enqueue_tmux_event(root, binding, tmux_shortcut_key_event(key), cx) {
                root.status = Some(crate::locale::text(
                    "tmux 前缀已发送，但后续按键未能排队。",
                    "The tmux prefix was sent, but the following key could not be queued.",
                ));
                cx.notify();
            }
        }
    }
}

/// 字符快捷键携带同一次按键的文本；legacy 终端不会为裸 Character 事件生成字符，不能只排队一个空事件。
/// 同时保留物理键身份，让 Kitty 模式可按当前协议编码。
///
/// @author kongweiguang
fn tmux_shortcut_key_event(key: char) -> TerminalInputEvent {
    TerminalInputEvent::KeyWithText {
        event: TerminalKeyEvent {
            key: TerminalKey::Character(key),
            phase: KeyPhase::Press,
            modifiers: InputModifiers::default(),
        },
        text: key.to_string(),
        alternates: Default::default(),
    }
}

/// 将事件强制标为 Invalidates；合成快捷项不能污染命令行正文，work 只装箱一次进入 FIFO。
///
/// @author kongweiguang
fn enqueue_tmux_event(
    root: &mut RootView,
    binding: &UiSessionBinding,
    event: TerminalInputEvent,
    cx: &mut Context<RootView>,
) -> bool {
    root.enqueue_session_command(
        SessionCommand::UiInput(Box::new(super::session_queue::UiInputWork::new(
            UiTerminalInputRequest {
                binding: binding.clone(),
                expected_context: None,
                projection: UiInputProjection::Invalidates,
                event,
            },
            None,
        ))),
        cx,
    )
}

/// 当前 workspace 必须仍是同一聚焦 Terminal pane，且 runtime、handler 和持久化 tmux 绑定三者一致。
///
/// @author kongweiguang
fn tmux_quickref_is_current(
    root: &RootView,
    pane_id: &kerminal_domain::identity::PaneId,
    binding: &UiSessionBinding,
    target: &TmuxTarget,
    _session_id: &str,
) -> bool {
    if root.center_panel != CenterPanel::Terminal {
        return false;
    }
    let Some(controller) = root.workspace_controller.as_ref() else {
        return false;
    };
    let snapshot = controller.snapshot();
    let Some(tab_id) = snapshot.active_tab_id.as_ref() else {
        return false;
    };
    if snapshot.focused_pane_id.as_ref() != Some(pane_id)
        || !snapshot.terminal_tabs.iter().any(|tab| {
            tab.id() == tab_id
                && tab
                    .terminal_layout()
                    .is_some_and(|layout| layout.contains_pane(pane_id))
        })
    {
        return false;
    }
    let Some(pane) = root.workspace_runtime.pane(pane_id) else {
        return false;
    };
    let expected_owner = SessionOwner::Ui {
        tab_id: tab_id.clone(),
        pane_id: pane_id.clone(),
    };
    let Some(session) = pane.session_id.as_ref() else {
        return false;
    };
    let Some(registration) = root.pane_input_handlers.get(session) else {
        return false;
    };
    let target_is_same_transport = matches!(
        (pane.target.as_ref(), pane.persisted.target.as_ref()),
        (Some(SessionTarget::Ssh(actual)), Some(WorkspaceTargetRef::Ssh { host_id }))
            if &actual.host_id == host_id
    ) || matches!(
        (pane.target.as_ref(), pane.persisted.target.as_ref()),
        (
            Some(SessionTarget::Local(_)),
            Some(WorkspaceTargetRef::Local { .. })
        )
    );
    pane.tab_id == *tab_id
        && pane.phase == RuntimePanePhase::Ready
        && session == &binding.session_id
        && binding.owner == expected_owner
        && registration.binding == *binding
        && target_is_same_transport
        && focused_tmux_attach_binding(root)
            .is_some_and(|terminal| terminal.target.matches_tmux_target(target))
}

/// 比较 tmux attach 时保存的目标身份，不能因当前侧栏主机变化而把快捷内容转投到另一连接。
///
/// @author kongweiguang
fn tmux_binding_matches_request(
    binding: &WorkspaceTmuxBinding,
    target: &TmuxTarget,
    session_id: &str,
) -> bool {
    target.validate().is_ok()
        && binding.session_id == session_id
        && tmux_binding_target_ref(target).as_deref() == Some(binding.target_ref.as_str())
        && binding.socket_name == target.socket_name
        && binding.socket_path == target.socket_path
}

/// 限制 Quickref payload 为单行命令或 tmux 的 Ctrl-B 加单个可见按键。
///
/// @author kongweiguang
fn valid_quickref_text(text: &str, kind: TmuxQuickrefKind) -> bool {
    match kind {
        TmuxQuickrefKind::Command => {
            !text.is_empty()
                && text.len() <= 2_048
                && !text.chars().any(char::is_control)
                && !text
                    .chars()
                    .any(|character| matches!(character, '\r' | '\n'))
        }
        TmuxQuickrefKind::Shortcut => {
            let mut chars = text.chars();
            chars.next() == Some('\u{2}')
                && chars.next().is_some_and(|key| key.is_ascii_graphic())
                && chars.next().is_none()
        }
    }
}

/// 从 Root 当前显式焦点取 tmux quickref 的只读目标投影；缺少焦点或绑定时隐藏发送入口。
///
/// @author kongweiguang
pub(super) fn focused_tmux_quickref_binding(
    root: &RootView,
) -> Option<TmuxQuickrefTerminalBinding> {
    if root.center_panel != CenterPanel::Terminal {
        return None;
    }
    let snapshot = root.workspace_controller.as_ref()?.snapshot();
    let tab_id = snapshot.active_tab_id.as_ref()?;
    let pane_id = snapshot.focused_pane_id.as_ref()?;
    if !snapshot.terminal_tabs.iter().any(|tab| {
        tab.id() == tab_id
            && tab
                .terminal_layout()
                .is_some_and(|layout| layout.contains_pane(pane_id))
    }) {
        return None;
    }
    let pane = root.workspace_runtime.pane(pane_id)?;
    if pane.tab_id != *tab_id
        || pane.phase != RuntimePanePhase::Ready
        || !matches!(
            pane.target.as_ref(),
            Some(SessionTarget::Ssh(_) | SessionTarget::Local(_))
        )
    {
        return None;
    }
    let session_id = pane.session_id.as_ref()?;
    let binding = root.pane_input_handlers.get(session_id)?.binding.clone();
    if binding.session_id != *session_id
        || binding.owner
            != (SessionOwner::Ui {
                tab_id: tab_id.clone(),
                pane_id: pane_id.clone(),
            })
    {
        return None;
    }
    Some(TmuxQuickrefTerminalBinding {
        pane_id: pane_id.clone(),
        binding,
    })
}

/// 当前会话标记只接受精确 focused pane 的 tmux 持久绑定，不从面板最近点击的行猜测。
/// @author kongweiguang
pub(super) fn focused_tmux_current_session_id(root: &RootView) -> Option<String> {
    let terminal = focused_tmux_attach_binding(root)?;
    let pane = root.workspace_runtime.pane(&terminal.pane_id)?;
    let binding = pane.persisted.tmux_binding.as_ref()?;
    tmux_binding_matches_request(
        binding,
        &terminal.target.to_tmux_target(),
        &binding.session_id,
    )
    .then(|| binding.session_id.clone())
}

/// 名称仅用于 main 的当前行显示回退；沿用精确 pane/host 校验，命令执行仍使用快照中的会话 ID。
///
/// @author kongweiguang
pub(super) fn focused_tmux_current_session_name(root: &RootView) -> Option<String> {
    let terminal = focused_tmux_attach_binding(root)?;
    let pane = root.workspace_runtime.pane(&terminal.pane_id)?;
    let binding = pane.persisted.tmux_binding.as_ref()?;
    let name = binding.session_name.trim();
    (tmux_binding_matches_request(
        binding,
        &terminal.target.to_tmux_target(),
        &binding.session_id,
    ) && !name.is_empty())
    .then(|| binding.session_name.clone())
}

/// Project only the focused, Ready host-local or SSH pane; typed target and owner prevent cross-pane fallback.
///
/// @author kongweiguang
pub(super) fn focused_tmux_attach_binding(root: &RootView) -> Option<TmuxAttachTerminalBinding> {
    if root.center_panel != CenterPanel::Terminal {
        return None;
    }
    let snapshot = root.workspace_controller.as_ref()?.snapshot();
    let tab_id = snapshot.active_tab_id.as_ref()?;
    let pane_id = snapshot.focused_pane_id.as_ref()?;
    if !snapshot.terminal_tabs.iter().any(|tab| {
        tab.id() == tab_id
            && tab
                .terminal_layout()
                .is_some_and(|layout| layout.contains_pane(pane_id))
    }) {
        return None;
    }
    let pane = root.workspace_runtime.pane(pane_id)?;
    let target = match (pane.target.as_ref()?, pane.persisted.target.as_ref()?) {
        (SessionTarget::Ssh(actual), WorkspaceTargetRef::Ssh { host_id })
            if &actual.host_id == host_id
                && pane
                    .persisted
                    .remote_host_id
                    .as_ref()
                    .is_none_or(|persisted| persisted == host_id)
                && pane.persisted.profile_id.is_none() =>
        {
            TmuxAttachTarget::Ssh {
                host_id: actual.host_id.clone(),
            }
        }
        (SessionTarget::Local(actual), WorkspaceTargetRef::Local { profile_id })
            if pane.persisted.remote_host_id.is_none()
                && !tmux_local_shell_is_wsl(&actual.program)
                && pane.persisted.profile_id.as_ref() == profile_id.as_ref() =>
        {
            TmuxAttachTarget::Local {
                profile_id: profile_id.clone(),
            }
        }
        _ => return None,
    };
    if pane.phase != RuntimePanePhase::Ready || pane.tab_id != *tab_id {
        return None;
    }
    let session_id = pane.session_id.as_ref()?;
    let binding = root.pane_input_handlers.get(session_id)?.binding.clone();
    if binding.session_id != *session_id
        || binding.owner
            != (SessionOwner::Ui {
                tab_id: tab_id.clone(),
                pane_id: pane_id.clone(),
            })
    {
        return None;
    }
    Some(TmuxAttachTerminalBinding {
        pane_id: pane_id.clone(),
        target,
        binding,
        working_directory: pane
            .persisted
            .current_cwd
            .clone()
            .or_else(|| pane.persisted.cwd.clone()),
    })
}

/// Local tmux 的 service target 是宿主本机；拒绝直接启动 WSL 的 pane，避免把宿主列表发入 WSL tmux。
///
/// @author kongweiguang
fn tmux_local_shell_is_wsl(program: &OsStr) -> bool {
    let Some(program) = program.to_str() else {
        return false;
    };
    program.rsplit(['\\', '/']).next().is_some_and(|name| {
        name.eq_ignore_ascii_case("wsl.exe") || name.eq_ignore_ascii_case("wsl")
    })
}

/// 为容器文件面板构造端点，确保文件操作始终绑定发出事件的目标。
fn container_file_endpoint(
    target: &ContainerTarget,
    path: &str,
) -> Result<FileEndpoint, FilePathError> {
    Ok(FileEndpoint::Container {
        host_id: target.host_id.clone(),
        runtime: target.runtime,
        container_id: target.container_id.clone(),
        path: RemotePath::parse(path)?,
    })
}

/// 打开由 Operations 事件明确指定的容器目录；缺少服务或非法路径时给出可见错误。
///
/// @author kongweiguang
pub(super) fn open_container_files(
    root: &mut RootView,
    target: &ContainerTarget,
    path: &str,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    let endpoint = match container_file_endpoint(target, path) {
        Ok(endpoint) => endpoint,
        Err(_) => {
            root.status = Some(crate::locale::text(
                "容器路径无效。",
                "Invalid container path.",
            ));
            cx.notify();
            return;
        }
    };
    let Some(files_pane) = root.files_pane.clone() else {
        root.status = Some(crate::locale::text(
            "容器文件服务尚未配置。",
            "Container file service is unavailable.",
        ));
        cx.notify();
        return;
    };

    files_pane.update(cx, |pane, cx| pane.open_endpoint(endpoint, cx));
    root.open_shared_sftp_workbench(window, cx);
}

/// 将容器 exec 的参数转成可供 Root renderer 使用的 pane intent；一次性命令不写入恢复快照。
fn container_exec_intent(target: &ContainerTarget, command: &[String]) -> WorkspaceTerminalIntent {
    let workspace_target =
        target
            .host_id
            .as_ref()
            .map(|host_id| WorkspaceTargetRef::DockerContainer {
                host_id: host_id.clone(),
                container_id: target.container_id.clone(),
                runtime: match target.runtime {
                    ContainerRuntime::Docker => WorkspaceContainerRuntime::Docker,
                    ContainerRuntime::Podman => WorkspaceContainerRuntime::Podman,
                },
                container_name: None,
                user: None,
                workdir: None,
            });
    let machine_id = target.host_id.as_ref().map_or_else(
        || "machine-local-container".to_owned(),
        |host| host.as_str().to_owned(),
    );
    let shell = command.first().cloned();
    let args = Some(command.iter().skip(1).cloned().collect());
    let pane = workspace_pane(
        machine_id,
        WorkspacePaneMode::Container,
        workspace_target,
        target.host_id.clone(),
        Some(target.container_id.clone()),
        None,
        shell,
        args,
        None,
        None,
        None,
        None,
    );
    let title = crate::locale::text(
        format!("容器 {} 执行", short_id(&target.container_id)),
        format!("Container {} exec", short_id(&target.container_id)),
    )
    .to_string();
    WorkspaceTerminalIntent {
        pane,
        title,
        kind: WorkspaceTerminalOpenKind::ContainerExec {
            target: target.clone(),
            command: command.to_vec(),
        },
        // 自定义命令不可安全地从工作区快照重放，故本次 exec 始终为临时 pane。
        transient: true,
    }
}

/// 日志在新的宿主终端持续输出，标题沿用发起时的容器名称；命令不进入工作区恢复重放路径。
/// @author kongweiguang
fn container_logs_intent(target: &ContainerTarget, name: &str) -> WorkspaceTerminalIntent {
    let workspace_target = target
        .host_id
        .as_ref()
        .map(|host_id| WorkspaceTargetRef::Ssh {
            host_id: host_id.clone(),
        });
    let pane = workspace_pane(
        target
            .host_id
            .as_ref()
            .map_or_else(|| "machine-local".to_owned(), |id| id.as_str().to_owned()),
        if target.host_id.is_some() {
            WorkspacePaneMode::Ssh
        } else {
            WorkspacePaneMode::Local
        },
        workspace_target.or(Some(WorkspaceTargetRef::Local { profile_id: None })),
        target.host_id.clone(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    WorkspaceTerminalIntent {
        pane,
        title: format!("{name} logs"),
        kind: WorkspaceTerminalOpenKind::ContainerLogs {
            target: target.clone(),
        },
        transient: true,
    }
}

/// 只在精确 pane binding 和可信 shell 方言都匹配时向原 pane 排队；仅在命令和回车均排队后返回绑定，供宿主恢复原 pane 焦点。
///
/// @author kongweiguang
fn attach_tmux_in_current_terminal(
    root: &mut RootView,
    target: &TmuxTarget,
    session_id: &str,
    session_name: &str,
    terminal: Option<&TmuxAttachTerminalBinding>,
    operations_view: &Entity<crate::features::operations::OperationsView>,
    cx: &mut Context<RootView>,
) -> Option<UiSessionBinding> {
    if target.validate().is_err() || !valid_tmux_session_id(session_id) {
        root.status = Some(crate::locale::text(
            "tmux 会话目标无效，未发送连接命令。",
            "Invalid tmux session target; no command was sent.",
        ));
        cx.notify();
        return None;
    }
    let Some(terminal) = terminal else {
        root.status = Some(crate::locale::text(
            "请先聚焦一个已就绪的本机或 SSH 终端，再连接 tmux 会话。",
            "Focus a ready local or SSH terminal before attaching to a tmux session.",
        ));
        cx.notify();
        return None;
    };
    let Some(binding) = current_tmux_terminal_binding(root, target, terminal) else {
        root.status = Some(crate::locale::text(
            "当前终端或目标已变化；tmux 命令未发送，请切回对应终端后重试。",
            "The focused terminal or target changed; tmux was not sent. Return to the matching terminal and retry.",
        ));
        cx.notify();
        return None;
    };
    let Some(shell_dialect) = tmux_shell_dialect_for_binding(root, &binding) else {
        root.status = Some(crate::locale::text(
            "无法确认当前终端的 shell 方言，不能安全生成 tmux 命令；未发送。",
            "The focused terminal shell is not confirmed, so no tmux command was sent.",
        ));
        cx.notify();
        return None;
    };
    if shell_dialect == RemoteShellDialect::Unknown {
        root.status = Some(crate::locale::text(
            "无法确认当前终端的 shell 方言，不能安全生成 tmux 命令；未发送。",
            "The focused terminal shell is not confirmed, so no tmux command was sent.",
        ));
        cx.notify();
        return None;
    }
    let Some(command) = tmux_attach_command(target, session_id, shell_dialect) else {
        root.status = Some(crate::locale::text(
            "无法为当前终端安全编码 tmux 命令；未发送。",
            "The tmux command could not be encoded safely for this terminal; nothing was sent.",
        ));
        cx.notify();
        return None;
    };
    let pane = root.workspace_runtime.pane(&terminal.pane_id);
    if let Some(current_tmux) = pane.and_then(|pane| pane.persisted.tmux_binding.as_ref()) {
        if tmux_binding_matches_request(current_tmux, target, session_id) {
            root.status = Some(crate::locale::text(
                "当前终端已经连接此 tmux 会话。",
                "This terminal is already attached to that tmux session.",
            ));
        } else {
            root.status = Some(crate::locale::text(
                "当前终端已连接其他 tmux 会话，请先在终端中分离后再切换。",
                "This terminal is attached to another tmux session. Detach there before switching.",
            ));
        }
        cx.notify();
        return None;
    }
    if !enqueue_tmux_event(root, &binding, TerminalInputEvent::Paste(command), cx) {
        root.status = Some(crate::locale::text(
            "当前终端暂时不能接收输入，连接命令未发送。",
            "The focused terminal cannot accept input; the attach command was not sent.",
        ));
        cx.notify();
        return None;
    }
    if !enqueue_tmux_event(
        root,
        &binding,
        TerminalInputEvent::Key(TerminalKeyEvent {
            key: TerminalKey::Enter,
            phase: KeyPhase::Press,
            modifiers: InputModifiers::default(),
        }),
        cx,
    ) {
        root.status = Some(crate::locale::text(
            "连接命令已写入，但提交键未能排队；请检查终端后再操作。",
            "The attach command was written, but Enter could not be queued. Check the terminal before continuing.",
        ));
        cx.notify();
        return None;
    }
    operations_view.update(cx, |view, view_cx| {
        view.report_tmux_attach_queued(target, session_id, session_name, terminal, view_cx);
    });
    cx.notify();
    Some(binding)
}

/// 分离只向原 pane 的 FIFO 发送默认前缀按键，不以主机 clients 数猜测 client，也不在未知 shell 提交 detach 命令。
/// 点击时冻结的 pane owner/generation 在入队前再次核对；失败和半次按键不会自动改投或重放。
///
/// @author kongweiguang
fn detach_tmux_in_current_terminal(
    root: &mut RootView,
    target: &TmuxTarget,
    terminal: &TmuxAttachTerminalBinding,
    operations_view: &Entity<OperationsView>,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    let binding = current_tmux_terminal_binding(root, target, terminal);
    if root.tool_panel_overlay_focus_is_blocked(window, cx)
        || binding.is_none()
        || !operations_view
            .read(cx)
            .tmux_detach_request_is_current(target, terminal)
    {
        operations_view.update(cx, |view, cx| {
            view.report_tmux_detach_current_result(target, terminal, false, cx);
        });
        root.status = Some(crate::locale::text(
            "当前终端或目标已变化；tmux 分离按键未发送，请重新核对。",
            "The terminal or target changed; no tmux detach keys were sent. Review the target again.",
        ));
        cx.notify();
        return;
    }
    let binding = binding.expect("validated current terminal binding");
    for event in tmux_detach_key_events() {
        if !enqueue_tmux_event(root, &binding, event, cx) {
            operations_view.update(cx, |view, cx| {
                view.report_tmux_detach_current_result(target, terminal, false, cx);
            });
            root.status = Some(crate::locale::text(
                "分离按键未能完整排队；请核对原终端，不会自动重放。",
                "The detach sequence could not be fully queued. Inspect the original terminal; it will not be replayed.",
            ));
            cx.notify();
            return;
        }
    }
    operations_view.update(cx, |view, cx| {
        view.report_tmux_detach_current_result(target, terminal, true, cx);
    });
    if let SessionOwner::Ui { tab_id, pane_id } = &binding.owner {
        root.focused_agent_terminal = false;
        root.request_workspace_pane_focus(tab_id.clone(), pane_id.clone(), window, cx);
    }
    cx.notify();
}

/// 按 main 的分离行为生成同一 FIFO 中的 Ctrl-B 与 d 两个按键事件，不发送 shell 命令。
///
/// @author kongweiguang
fn tmux_detach_key_events() -> [TerminalInputEvent; 2] {
    [
        TerminalInputEvent::Key(TerminalKeyEvent {
            key: TerminalKey::Character('b'),
            phase: KeyPhase::Press,
            modifiers: InputModifiers {
                control: true,
                ..InputModifiers::default()
            },
        }),
        tmux_shortcut_key_event('d'),
    ]
}

/// 只读取与活动 session、owner 和 generation 完全一致的服务上下文，禁止按本机或 SSH 猜测方言。
///
/// @author kongweiguang
fn tmux_shell_dialect_for_binding(
    root: &RootView,
    binding: &UiSessionBinding,
) -> Option<RemoteShellDialect> {
    let cached = root.terminal_command_contexts.get(&binding.session_id)?;
    (cached.binding == *binding
        && cached.context.session_id == binding.session_id
        && cached.context.owner == binding.owner
        && cached.context.session_generation == binding.session_generation)
        .then_some(cached.context.shell_dialect)
}

/// 将 Root 此刻验证过的 pane owner 与按键队列绑定，目标变化时拒绝使用缓存投影。
///
/// @author kongweiguang
fn current_tmux_terminal_binding(
    root: &RootView,
    target: &TmuxTarget,
    expected: &TmuxAttachTerminalBinding,
) -> Option<UiSessionBinding> {
    if target.validate().is_err()
        || !expected.target.matches_tmux_target(target)
        || root.center_panel != CenterPanel::Terminal
    {
        return None;
    }
    let snapshot = root.workspace_controller.as_ref()?.snapshot();
    let tab_id = snapshot.active_tab_id.as_ref()?;
    let pane_id = snapshot.focused_pane_id.as_ref()?;
    if pane_id != &expected.pane_id
        || !snapshot.terminal_tabs.iter().any(|tab| {
            tab.id() == tab_id
                && tab
                    .terminal_layout()
                    .is_some_and(|layout| layout.contains_pane(pane_id))
        })
    {
        return None;
    }
    let pane = root.workspace_runtime.pane(pane_id)?;
    let session_id = pane.session_id.as_ref()?;
    let registration = root.pane_input_handlers.get(session_id)?;
    let owner = SessionOwner::Ui {
        tab_id: tab_id.clone(),
        pane_id: pane_id.clone(),
    };
    let persisted_host_matches = match expected.target.host_id() {
        Some(host_id) => pane
            .persisted
            .remote_host_id
            .as_ref()
            .is_none_or(|persisted| persisted == host_id),
        None => pane.persisted.remote_host_id.is_none(),
    };
    if pane.tab_id != *tab_id
        || pane.phase != RuntimePanePhase::Ready
        || !persisted_host_matches
        || !tmux_target_matches_terminal(
            target,
            pane.target.as_ref()?,
            pane.persisted.target.as_ref()?,
        )
        || session_id != &expected.binding.session_id
        || expected.binding.owner != owner
        || registration.binding != expected.binding
    {
        return None;
    }
    Some(registration.binding.clone())
}

/// Exact host/profile and persisted workspace target must agree with the runtime transport.
///
/// @author kongweiguang
fn tmux_target_matches_terminal(
    target: &TmuxTarget,
    session_target: &SessionTarget,
    workspace_target: &WorkspaceTargetRef,
) -> bool {
    if target.validate().is_err() {
        return false;
    }
    match (target.host_id.as_ref(), session_target, workspace_target) {
        (Some(requested), SessionTarget::Ssh(actual), WorkspaceTargetRef::Ssh { host_id }) => {
            target.profile_id.is_none() && requested == &actual.host_id && requested == host_id
        }
        (None, SessionTarget::Local(_), WorkspaceTargetRef::Local { profile_id }) => {
            target.profile_id.as_ref() == profile_id.as_ref()
        }
        _ => false,
    }
}

/// 按当前终端的可信 shell 方言编码 argv；PowerShell 需显式调用符，Unknown 必须拒绝。
///
/// @author kongweiguang
fn tmux_attach_command(
    target: &TmuxTarget,
    session_id: &str,
    shell_dialect: RemoteShellDialect,
) -> Option<String> {
    if target.validate().is_err()
        || !valid_tmux_session_id(session_id)
        || shell_dialect == RemoteShellDialect::Unknown
    {
        return None;
    }
    let mut arguments = vec![
        target
            .tmux_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .unwrap_or("tmux")
            .to_owned(),
    ];
    if let Some(socket_name) = target.socket_name.as_deref() {
        arguments.extend(["-L".to_owned(), socket_name.to_owned()]);
    } else if let Some(socket_path) = target.socket_path.as_deref() {
        arguments.extend(["-S".to_owned(), socket_path.to_owned()]);
    }
    arguments.extend([
        "attach-session".to_owned(),
        "-t".to_owned(),
        session_id.to_owned(),
    ]);
    let arguments = arguments
        .iter()
        .map(|argument| crate::shell_arguments::quote_shell_argument(argument, shell_dialect))
        .collect::<Option<Vec<_>>>()?;
    let command = arguments.join(" ");
    Some(match shell_dialect {
        RemoteShellDialect::PowerShell => format!("& {command}"),
        RemoteShellDialect::Posix | RemoteShellDialect::Fish => command,
        RemoteShellDialect::Unknown => return None,
    })
}

/// Only accept tmux's stable `$N` session ID form so shell input cannot become an arbitrary command.
///
/// @author kongweiguang
fn valid_tmux_session_id(value: &str) -> bool {
    value.strip_prefix('$').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

/// 生成最小完整 workspace pane DTO；一次性创建数据由同一 reservation 分配身份。
#[allow(clippy::too_many_arguments)]
fn workspace_pane(
    machine_id: String,
    mode: WorkspacePaneMode,
    target: Option<WorkspaceTargetRef>,
    remote_host_id: Option<kerminal_domain::identity::HostId>,
    container_id: Option<String>,
    profile_id: Option<String>,
    shell: Option<String>,
    args: Option<Vec<String>>,
    cwd: Option<String>,
    remote_command: Option<String>,
    tmux_binding: Option<WorkspaceTmuxBinding>,
    local_machine_scope: Option<WorkspaceLocalMachineScope>,
) -> WorkspaceSessionPane {
    WorkspaceSessionPane {
        id: kerminal_domain::identity::PaneId::new(),
        title: String::new(),
        machine_id,
        mode,
        target,
        remote_host_id,
        container_id,
        profile_id,
        shell,
        args,
        cwd,
        current_cwd: None,
        env: None,
        remote_command,
        tmux_binding,
        prompt: String::new(),
        status: WorkspaceMachineStatus::Online,
        latency_ms: None,
        local_machine_scope,
        output_history: None,
    }
}

/// 让持久化的 tmux target_ref 与恢复端同构，拒绝无法表达的双重 socket 目标。
fn tmux_binding_target_ref(target: &TmuxTarget) -> Option<String> {
    let base = match (&target.host_id, &target.profile_id) {
        (Some(host_id), None) => format!("ssh:{host_id}"),
        (None, Some(profile_id)) => format!("local:{profile_id}"),
        (None, None) => "local".to_owned(),
        (Some(_), Some(_)) => return None,
    };
    let socket = match (&target.socket_name, &target.socket_path) {
        (Some(name), None) => format!("|L:{name}"),
        (None, Some(path)) => format!("|S:{path}"),
        (None, None) => String::new(),
        (Some(_), Some(_)) => return None,
    };
    Some(format!("{base}{socket}"))
}

/// 只用短 ID 提供标签识别度，不把完整容器标识拼入高频标题或错误消息。
///
/// @author kongweiguang
fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}

/// 通过预留回执调用 Root 唯一创建管线；同步拒绝时取消 reservation 并移除空 pane。
fn open_operation_terminal(
    root: &mut RootView,
    intent: WorkspaceTerminalIntent,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    let size: TerminalSize = super::terminal_size_for_window(
        window,
        super::recovery_banner_height(&root.configuration_recovery),
        &root.terminal_metrics,
    );
    let reservation = match root.reserve_new_workspace_terminal_tab(
        intent.pane,
        intent.title,
        size,
        intent.transient,
        cx,
    ) {
        Ok(reservation) => reservation,
        Err(error) => {
            root.status = Some(error);
            cx.notify();
            return;
        }
    };
    let pane_id = reservation.pane_id.clone();
    if let Err(error) = root.open_workspace_terminal_pane(
        super::terminal_open::WorkspaceTerminalOpenRequest {
            tab_id: reservation.tab_id,
            pane_id: pane_id.clone(),
            kind: intent.kind,
            size: reservation.size,
            lease: None,
            transient: reservation.transient,
            generation: reservation.generation,
            cancellation: reservation.cancellation,
        },
        cx,
    ) {
        root.cancel_workspace_restore(&pane_id);
        let _ = root.workspace_runtime.remove_empty_pane(&pane_id);
        root.status = Some(error);
        cx.notify();
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_operations.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/unit/root_container_operations.rs"]
mod container_operation_tests;
