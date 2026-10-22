//! Root 侧主机导航与已保存连接事件的轻量协调。
//!
//! @author kongweiguang

use gpui_kit::{AppContext as _, Context, PathPromptOptions, Window};
use kerminal_application::{
    AppError, AppResult,
    connections::RdpClientLifecycle,
    platform::external_launch::{ExternalLaunchIntent, ExternalRoutedTarget},
    workspace::{
        WorkspaceMachineStatus, WorkspacePaneMode, WorkspaceSessionPane, WorkspaceSessionTab,
        WorkspaceTargetRef,
    },
};
use kerminal_domain::{
    connections::RemoteProtocol,
    files::{FileEndpoint, RemotePath},
    identity::{HostId, PaneId, TabId},
    sessions::{SessionTarget, SshInitialCommand, SshSessionTarget, TerminalSize},
};

use super::workspace_files::sftp_transfer_tab_for_host;
use crate::features::connections::{
    ConnectionsEvent, HostFilePickerTarget, HostNavigationEvent, HostSessionProjection,
    HostSessionStatus,
};
use crate::features::workspace_controller::WorkspacePersistenceState;

use super::workspace_runtime::RuntimePanePhase;
use super::{CenterPanel, RootView, TerminalTargetRequest, WorkspaceTerminalOpenKind};

/// SFTP 预检可跨越指纹确认模态，但不能跨越用户后来的中央页面或标签导航。
pub(super) struct SftpNavigationRequest {
    pub(super) request_id: String,
    pub(super) navigation_generation: u64,
}

impl SftpNavigationRequest {
    /// 同时比较操作身份与中央导航代次；主机相同也不能接收上一轮请求的迟到结果。
    ///
    /// @author kongweiguang
    pub(super) fn accepts(&self, request_id: &str, navigation_generation: u64) -> bool {
        self.request_id == request_id && self.navigation_generation == navigation_generation
    }
}

/// 预检结果的消费状态；被新请求替代的回执必须静默丢弃，避免覆盖更新状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SftpNavigationReceiptDisposition {
    Accepted,
    NavigationChanged,
    Superseded,
    Missing,
}

/// 单一连接面板的 SFTP picker 保存回执只可回投到发起时的共享工作台实例。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PendingSftpHostCreation {
    pub(super) request_id: String,
    pub(super) workbench_tab_id: TabId,
    pub(super) navigation_generation: u64,
    pub(super) saved_host: Option<(HostId, bool)>,
}

/// 请求身份优先于导航校验；先到的旧回执不能消费新预检或覆盖其可见状态。
///
/// @author kongweiguang
pub(super) fn consume_sftp_navigation_receipt(
    pending: &mut Option<SftpNavigationRequest>,
    latest_request_id: Option<&str>,
    request_id: &str,
    navigation_generation: u64,
) -> SftpNavigationReceiptDisposition {
    if latest_request_id.is_some_and(|latest| latest != request_id)
        || pending
            .as_ref()
            .is_some_and(|current| current.request_id != request_id)
    {
        return SftpNavigationReceiptDisposition::Superseded;
    }
    let Some(current) = pending.take() else {
        return SftpNavigationReceiptDisposition::Missing;
    };
    if current.accepts(request_id, navigation_generation) {
        SftpNavigationReceiptDisposition::Accepted
    } else {
        SftpNavigationReceiptDisposition::NavigationChanged
    }
}

/// 持久传输标签请求成功后，切页只影响是否激活，不应丢弃用户明确创建的结果。
///
/// @author kongweiguang
pub(super) fn sftp_transfer_tab_activation(
    disposition: SftpNavigationReceiptDisposition,
) -> Option<bool> {
    match disposition {
        SftpNavigationReceiptDisposition::Accepted => Some(true),
        SftpNavigationReceiptDisposition::NavigationChanged => Some(false),
        SftpNavigationReceiptDisposition::Superseded
        | SftpNavigationReceiptDisposition::Missing => None,
    }
}

/// 只有请求、共享工作台身份和中央导航代次都仍匹配时，保存完成才可启动自动预检。
///
/// @author kongweiguang
fn sftp_host_creation_source_is_current(
    pending: &PendingSftpHostCreation,
    request_id: &str,
    current_workbench_tab_id: &TabId,
    navigation_generation: u64,
    workbench_selected: bool,
) -> bool {
    pending.request_id == request_id
        && &pending.workbench_tab_id == current_workbench_tab_id
        && pending.navigation_generation == navigation_generation
        && workbench_selected
}

impl RootView {
    /// 只允许共享文件工作台发起 SFTP 默认的新主机表单，并撤销表单背后的旧共享预检。
    ///
    /// @author kongweiguang
    pub(super) fn begin_create_sftp_host_from_workbench(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.shared_sftp_workbench_is_selected() {
            return false;
        }
        let Some(panel) = self.connections_panel.clone() else {
            self.status = Some(crate::locale::text(
                "主机连接服务尚未配置。",
                "Host connection services are unavailable.",
            ));
            cx.notify();
            return false;
        };
        if !self.prepare_connection_editor_modal(window, cx) {
            return false;
        }

        // 打开新建表单代表用户离开当前 endpoint 操作；旧共享预检不得在表单背后改绑。
        self.surface_navigation_generation = self.surface_navigation_generation.wrapping_add(1);
        self.pending_sftp_navigation = None;
        let request_id = uuid::Uuid::new_v4().to_string();
        self.pending_sftp_host_creation = Some(PendingSftpHostCreation {
            request_id: request_id.clone(),
            workbench_tab_id: self.sftp_workbench_tab_id.clone(),
            navigation_generation: self.surface_navigation_generation,
            saved_host: None,
        });
        let started = panel.update(cx, |panel, cx| {
            panel.begin_create_sftp_host(request_id, window, cx)
        });
        if !started {
            self.pending_sftp_host_creation = None;
            self.close_connection_editor_modal(window, cx);
            self.status = Some(crate::locale::text(
                "连接编辑器正在处理其他草稿，暂时无法新建 SFTP 主机。",
                "The connection editor is handling another draft and cannot start an SFTP host yet.",
            ));
            cx.notify();
        }
        started
    }

    /// 保存回执和模态观察器共享同一恢复入口；模态关闭前保留资格，关闭后最多消费一次。
    ///
    /// @author kongweiguang
    pub(super) fn resume_sftp_host_creation(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.pending_sftp_host_creation.as_ref() else {
            return;
        };
        let Some((host_id, can_open_sftp)) = pending.saved_host.clone() else {
            return;
        };
        let request_id = pending.request_id.clone();
        if !sftp_host_creation_source_is_current(
            pending,
            &request_id,
            &self.sftp_workbench_tab_id,
            self.surface_navigation_generation,
            self.shared_sftp_workbench_is_selected(),
        ) {
            self.pending_sftp_host_creation = None;
            return;
        }
        if self.connections_modal_open || self.connection_prompt_modal_open {
            return;
        }

        self.pending_sftp_host_creation = None;
        if !can_open_sftp {
            self.status = Some(crate::locale::text(
                "主机已保存，但未启用 SFTP；可稍后从主机选择器中打开其他主机。",
                "The host was saved without SFTP enabled. You can choose another host from the picker.",
            ));
            cx.notify();
            return;
        }
        if let Some(panel) = self.connections_panel.clone() {
            panel.update(cx, |panel, cx| panel.request_open_sftp_host(&host_id, cx));
        } else {
            self.status = Some(crate::locale::text(
                "主机已保存，但连接服务已关闭；请从选择器重新打开 SFTP。",
                "The host was saved, but the connection service closed. Reopen SFTP from the picker.",
            ));
            cx.notify();
        }
    }
}

/// 将连接动作和原生文件选择器交给唯一 Root owner；选择器回执仍受编辑器代次约束。
///
/// 返回值供 Root 事件桥维持既有分发契约；迟到共享打开会被拒绝，独立传输成功可在后台落 Tab。
///
/// @author kongweiguang
pub(super) fn handle_connections_event(
    root: &mut RootView,
    event: &ConnectionsEvent,
    window: &mut Window,
    cx: &mut Context<RootView>,
) -> bool {
    match event {
        ConnectionsEvent::PickHostFile { generation, target } => {
            let Some(panel) = root.connections_panel.clone() else {
                return true;
            };
            let generation = *generation;
            let target = *target;
            let window_handle = window.window_handle();
            let selected_paths = cx.prompt_for_paths(host_file_picker_options(target));
            cx.spawn(async move |_, cx| {
                let selection = selected_paths.await;
                let _ = cx.update_window(window_handle, move |_, window, app| {
                    panel.update(app, |panel, cx| match selection {
                        Ok(Ok(Some(paths))) => {
                            let path = paths.into_iter().next();
                            panel.set_host_file_path(generation, target, path, window, cx);
                        }
                        Ok(Ok(None)) => {
                            panel.set_host_file_path(generation, target, None, window, cx);
                        }
                        Ok(Err(_)) => {
                            panel.set_host_file_picker_error(
                                generation,
                                target,
                                crate::locale::text(
                                    "系统文件选择器暂不可用，请检查桌面文件选择服务。",
                                    "The system file picker is unavailable. Check the desktop file picker service.",
                                ).to_string(),
                                cx,
                            );
                        }
                        Err(_) => {
                            panel.set_host_file_picker_error(
                                generation,
                                target,
                                crate::locale::text(
                                    "文件选择未能完成，请重试。",
                                    "File selection did not complete. Try again.",
                                ).to_string(),
                                cx,
                            );
                        }
                    });
                });
            })
            .detach();
            true
        }
        ConnectionsEvent::HostSelected { host_id } => {
            root.group_rename_return_surface = None;
            root.selected_host_id = Some(host_id.clone());
            root.refresh_operations_panel_targets(cx);
            cx.notify();
            true
        }
        ConnectionsEvent::HostSaved { host_id } => {
            root.group_rename_return_surface = None;
            root.selected_host_id = Some(host_id.clone());
            root.refresh_operations_panel_targets(cx);
            if let Some(navigation) = root.host_navigation.clone() {
                navigation.update(cx, |navigation, cx| {
                    navigation.select_saved_host(host_id.clone(), cx)
                });
            }
            cx.notify();
            true
        }
        ConnectionsEvent::LocalProfileSaved { profile_id } => {
            root.group_rename_return_surface = None;
            root.selected_host_id = None;
            root.refresh_operations_panel_targets(cx);
            if let Some(navigation) = root.host_navigation.clone() {
                navigation.update(cx, |navigation, cx| {
                    navigation.select_saved_profile(profile_id.clone(), cx)
                });
            }
            cx.notify();
            true
        }
        ConnectionsEvent::ConnectRequested { host_id, target } => {
            root.create_terminal_for_target(
                TerminalTargetRequest::ExplicitHost {
                    host_id: host_id.clone(),
                    target: target.clone(),
                },
                window,
                cx,
            );
            true
        }
        ConnectionsEvent::ConnectLocalProfileRequested { profile_id } => {
            // 本地主机仍由 ProfileService 按稳定 ProfileId 解析；不能把它伪装成远端 HostId。
            root.selected_host_id = None;
            root.refresh_operations_panel_targets(cx);
            root.create_terminal_for_target(local_profile_target_request(profile_id), window, cx);
            true
        }
        ConnectionsEvent::CreateSftpTransferTabRequested {
            request_id,
            endpoint,
            initial_local_directory,
            title,
        } => {
            let disposition = consume_sftp_navigation_receipt(
                &mut root.pending_sftp_transfer_navigation,
                root.latest_sftp_request_id.as_deref(),
                request_id,
                root.surface_navigation_generation,
            );
            if let Some(should_activate) = sftp_transfer_tab_activation(disposition) {
                let created = create_sftp_transfer_workspace_tab(
                    root,
                    endpoint,
                    title,
                    initial_local_directory.as_deref(),
                    should_activate,
                    window,
                    cx,
                );
                if created && !should_activate {
                    root.status = Some(crate::locale::text(
                        "传输标签已在后台创建，当前页面保持不变。",
                        "The transfer tab was created in the background; the current page stayed open.",
                    ));
                    cx.notify();
                }
            }
            true
        }
        ConnectionsEvent::GroupRenameCancelled | ConnectionsEvent::GroupRenameSaved => {
            finish_group_rename_return(root, window, cx);
            true
        }
        ConnectionsEvent::SftpOpenStarted { request_id } => {
            root.latest_sftp_request_id = Some(request_id.clone());
            root.pending_sftp_transfer_navigation = None;
            root.pending_sftp_host_creation = None;
            root.pending_sftp_navigation = Some(SftpNavigationRequest {
                request_id: request_id.clone(),
                navigation_generation: root.surface_navigation_generation,
            });
            true
        }
        ConnectionsEvent::SftpTransferTabOpenStarted { request_id } => {
            root.latest_sftp_request_id = Some(request_id.clone());
            root.pending_sftp_navigation = None;
            root.pending_sftp_host_creation = None;
            root.pending_sftp_transfer_navigation = Some(SftpNavigationRequest {
                request_id: request_id.clone(),
                navigation_generation: root.surface_navigation_generation,
            });
            true
        }
        ConnectionsEvent::SftpHostCreationSaved {
            request_id,
            host_id,
            can_open_sftp,
        } => {
            if let Some(navigation) = root.host_navigation.clone() {
                navigation.update(cx, |navigation, cx| navigation.reload(cx));
            }
            if let Some(pending) = root
                .pending_sftp_host_creation
                .as_mut()
                .filter(|pending| pending.request_id == *request_id)
            {
                pending.saved_host = Some((host_id.clone(), *can_open_sftp));
                root.resume_sftp_host_creation(window, cx);
            }
            true
        }
        ConnectionsEvent::SftpHostCreationCancelled { request_id } => {
            if root
                .pending_sftp_host_creation
                .as_ref()
                .is_some_and(|pending| pending.request_id == *request_id)
            {
                root.pending_sftp_host_creation = None;
            }
            true
        }
        ConnectionsEvent::OpenSftpRequested {
            request_id,
            endpoint,
            initial_local_directory,
        } => {
            let disposition = consume_sftp_navigation_receipt(
                &mut root.pending_sftp_navigation,
                root.latest_sftp_request_id.as_deref(),
                request_id,
                root.surface_navigation_generation,
            );
            if disposition != SftpNavigationReceiptDisposition::Accepted {
                if !matches!(
                    disposition,
                    SftpNavigationReceiptDisposition::Superseded
                        | SftpNavigationReceiptDisposition::Missing
                ) {
                    // 预检未创建文件浏览资源，迟到结果可安全拒绝，无需重绑或取消其它工作台任务。
                    root.status = Some(crate::locale::text(
                        "已离开原 SFTP 打开流程，当前页面保持不变；可重新选择主机。",
                        "Navigation changed while SFTP was preparing. Select the host again to open it.",
                    ));
                    cx.notify();
                }
                return true;
            }
            let Some(files) = root.files_pane.clone() else {
                root.status = Some(crate::locale::text(
                    "文件服务尚未配置，无法打开 SFTP 工作区。",
                    "File services are unavailable, so the SFTP workspace cannot be opened.",
                ));
                cx.notify();
                return true;
            };
            let endpoint = endpoint.clone();
            let initial_local_directory = initial_local_directory.clone();
            files.update(cx, |pane, cx| {
                pane.open_host_endpoint(endpoint, initial_local_directory, cx);
            });
            root.open_shared_sftp_workbench(window, cx);
            true
        }
        ConnectionsEvent::RdpLaunchCompleted { receipt, .. } => {
            let message = match receipt.lifecycle {
                RdpClientLifecycle::ManagedProcess => crate::locale::text(
                    "RDP 客户端进程已启动。",
                    "The managed RDP client process has started.",
                ),
                RdpClientLifecycle::DelegatedToSystem => crate::locale::text(
                    "已请求系统打开 RDP 文件；登录状态由外部客户端处理。",
                    "The system was asked to open the RDP file; the external client handles sign-in.",
                ),
            };
            root.status = Some(message);
            cx.notify();
            true
        }
        ConnectionsEvent::ExternalTargetReady { target } => {
            handle_external_target(root, target, window, cx)
        }
    }
}

/// 按用途限制原生选择器类型，避免目录和可执行文件被误选后再走文本路径转换。
///
/// @author kongweiguang
fn host_file_picker_options(target: HostFilePickerTarget) -> PathPromptOptions {
    let (files, directories, prompt) = match target {
        HostFilePickerTarget::PrivateKey => (
            true,
            false,
            crate::locale::text("选择 SSH 私钥", "Select an SSH private key"),
        ),
        HostFilePickerTarget::LocalShell => (
            true,
            false,
            crate::locale::text("选择 Shell 可执行文件", "Select a shell executable"),
        ),
        HostFilePickerTarget::LocalWorkingDirectory => (
            false,
            true,
            crate::locale::text("选择工作目录", "Select a working directory"),
        ),
        HostFilePickerTarget::SftpLocalDirectory => (
            false,
            true,
            crate::locale::text("选择本地传输目录", "Select a local transfer directory"),
        ),
    };
    PathPromptOptions {
        files,
        directories,
        multiple: false,
        prompt: Some(prompt),
    }
}

/// 按外部请求的明确意图分流；SFTP 和 SSH terminal 分别交给各自 lease owner 接管。
fn handle_external_target(
    root: &mut RootView,
    target: &ExternalRoutedTarget,
    window: &mut Window,
    cx: &mut Context<RootView>,
) -> bool {
    match &target.intent {
        ExternalLaunchIntent::SftpTransfer { .. } => handle_external_sftp(root, target, window, cx),
        ExternalLaunchIntent::SshTerminal { remote_command } => {
            handle_external_ssh_terminal(root, target, remote_command, window, cx)
        }
    }
}

/// 把经过密码与逐跳 host-key 检查的 SFTP 目标交给文件面板，并让面板先独立 retain lease。
///
/// 连接事件携带的 lease 只在本次交接期间借用；面板返回成功前不会假设它已经接管运行时目标，成功后显式激活共享工作台 Tab。
///
/// @author kongweiguang
fn handle_external_sftp(
    root: &mut RootView,
    target: &ExternalRoutedTarget,
    window: &mut Window,
    cx: &mut Context<RootView>,
) -> bool {
    let ExternalLaunchIntent::SftpTransfer { remote_path, .. } = &target.intent else {
        return show_external_route_error(root, "外部请求的意图与 SFTP 工作区不匹配。", cx);
    };
    if target.identity.protocol != RemoteProtocol::Sftp {
        return show_external_route_error(root, "外部请求的协议与 SFTP 动作不匹配。", cx);
    }
    let path = match RemotePath::parse(remote_path) {
        Ok(path) => path,
        Err(_) => return show_external_route_error(root, "外部 SFTP 路径无效。", cx),
    };
    let endpoint = FileEndpoint::Remote {
        host_id: target.host_id().clone(),
        path,
    };
    let Some(files) = root.files_pane.clone() else {
        return show_external_route_error(root, "文件服务尚未配置，无法打开 SFTP 工作区。", cx);
    };

    match files.update(cx, |pane, cx| {
        pane.open_external_endpoint(endpoint, &target.lease, cx)
    }) {
        Ok(()) => {
            root.open_shared_sftp_workbench(window, cx);
            root.status = None;
            cx.notify();
        }
        Err(error) => {
            root.status = Some(crate::locale::text(
                error.public_message(),
                "The external SFTP workspace could not take ownership of this target.",
            ));
            cx.notify();
        }
    }
    true
}

/// 展示已脱敏的外部路由失败原因；失败前没有消费者接管 lease，可由事件生命周期正常释放。
fn show_external_route_error(
    root: &mut RootView,
    message: &'static str,
    cx: &mut Context<RootView>,
) -> bool {
    root.status = Some(crate::locale::text(
        message,
        "The external connection request could not be opened.",
    ));
    cx.notify();
    true
}

/// 保留外部 SSH 命令的 argv 边界，避免把不可信参数拼成远端 shell 字符串。
fn external_ssh_session_target(
    host_id: &HostId,
    remote_command: &[String],
) -> AppResult<SessionTarget> {
    let command = match remote_command.split_first() {
        None => None,
        Some((program, args)) if !program.is_empty() => Some(SshInitialCommand::Process {
            program: program.clone(),
            args: args.to_vec(),
        }),
        Some(_) => return Err(AppError::invalid("外部 SSH 命令缺少可执行程序")),
    };

    Ok(SessionTarget::Ssh(SshSessionTarget {
        host_id: host_id.clone(),
        working_directory: None,
        command,
    }))
}

/// 预留 transient workspace pane 并把独立 lease 交给 Root session owner。
///
/// 外部命令只进运行态 `SshSessionTarget`；持久 pane 仅存外部 HostId 且由 save projection 剔除，
/// 因而命令 argv 与临时凭据不会进入 workspace v3 文件。
fn handle_external_ssh_terminal(
    root: &mut RootView,
    target: &ExternalRoutedTarget,
    remote_command: &[String],
    window: &mut Window,
    cx: &mut Context<RootView>,
) -> bool {
    if target.identity.protocol != RemoteProtocol::Ssh {
        return show_external_route_error(root, "外部请求的协议与 SSH 终端动作不匹配。", cx);
    }
    if root.workspace_controller.is_none() || root.workspace_load_pending {
        return show_external_route_error(root, "工作区尚未就绪，无法安全打开临时终端。", cx);
    }

    let session_target = match external_ssh_session_target(target.host_id(), remote_command) {
        Ok(target) => target,
        Err(error) => {
            root.status = Some(error.public_message().into());
            cx.notify();
            return true;
        }
    };
    let pane_lease = match target.lease.retain() {
        Ok(lease) => lease,
        Err(_) => return show_external_route_error(root, "外部连接租约已失效，未创建终端。", cx),
    };

    let title = external_terminal_title(target);
    let pane = external_ssh_workspace_pane(target.host_id(), &title);
    let reservation = match root.reserve_new_workspace_terminal_tab(
        pane,
        title.clone(),
        TerminalSize::default(),
        true,
        cx,
    ) {
        Ok(reservation) => reservation,
        Err(error) => {
            root.status = Some(error);
            cx.notify();
            return true;
        }
    };

    if root
        .workspace_runtime
        .set_restore_target(&reservation.pane_id, session_target.clone())
        .is_err()
    {
        discard_external_reservation(root, &reservation.pane_id, &reservation.cancellation, cx);
        return show_external_route_error(root, "临时终端目标无法登记，未启动连接。", cx);
    }
    if root
        .workspace_runtime
        .set_open_sftp_after_connect(&reservation.pane_id, target.open_sftp_after_connect)
        .is_err()
    {
        discard_external_reservation(root, &reservation.pane_id, &reservation.cancellation, cx);
        return show_external_route_error(root, "SFTP 后续操作无法登记，未启动连接。", cx);
    }

    let Some(mut snapshot) = root
        .workspace_controller
        .as_ref()
        .map(|controller| controller.snapshot().clone())
    else {
        discard_external_reservation(root, &reservation.pane_id, &reservation.cancellation, cx);
        return show_external_route_error(root, "工作区状态已变化，临时终端未启动。", cx);
    };
    snapshot.terminal_tabs.push(WorkspaceSessionTab::Terminal {
        id: reservation.tab_id.clone(),
        tab_group_id: None,
        title,
        machine_id: target.host_id().as_str().to_owned(),
        layout: reservation.layout.clone(),
    });
    snapshot.terminal_panes.push(reservation.pane.clone());
    snapshot.active_tab_id = Some(reservation.tab_id.clone());
    snapshot.focused_pane_id = Some(reservation.pane_id.clone());
    snapshot.selected_machine_id = Some(target.host_id().as_str().to_owned());

    let synchronized = root
        .workspace_controller
        .as_mut()
        .ok_or_else(|| AppError::conflict("工作区状态已变化"))
        .and_then(|controller| controller.synchronize_snapshot(snapshot));
    if !matches!(
        synchronized,
        Ok(crate::features::workspace_controller::WorkspaceControllerEffect::Changed { .. })
    ) {
        discard_external_reservation(root, &reservation.pane_id, &reservation.cancellation, cx);
        return show_external_route_error(root, "工作区正在处理其它变更，临时终端未启动。", cx);
    }

    root.set_center_panel(CenterPanel::Terminal, window, cx);
    root.refresh_workspace_views(cx);
    let opened = root.open_workspace_terminal_pane(
        super::terminal_open::WorkspaceTerminalOpenRequest {
            tab_id: reservation.tab_id,
            pane_id: reservation.pane_id.clone(),
            kind: WorkspaceTerminalOpenKind::Direct(session_target.clone()),
            size: reservation.size,
            lease: Some(pane_lease),
            transient: reservation.transient,
            generation: reservation.generation,
            cancellation: reservation.cancellation.clone(),
        },
        cx,
    );
    match opened {
        Ok(()) => {
            root.status = Some(crate::locale::text(
                "正在连接外部 SSH 主机。",
                "Connecting to the external SSH host.",
            ));
        }
        Err(error) => {
            let _ = root
                .workspace_runtime
                .mark_restore_failed(&reservation.pane_id);
            root.status = Some(error);
        }
    }
    root.persist_workspace_snapshot(cx);
    cx.notify();
    true
}

/// 首次 Ready ACK 后先消费外部请求的一次性 SFTP 意图，再尝试独立打开文件工作区。
///
/// 一旦开始 follow-up 就不因缺服务、失效 lease 或 SFTP 错误在重连时自动重放；
/// 外部文件面板保留自己的 lease，失败只显示可见错误，不影响已就绪 SSH pane。
///
/// @author kongweiguang
pub(super) fn open_external_sftp_after_connect(
    root: &mut RootView,
    pane_id: &PaneId,
    session_id: &kerminal_domain::identity::SessionId,
    generation: u64,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    if !root.workspace_restore_is_current(pane_id, generation) {
        return;
    }
    let Some(pane) = root.workspace_runtime.pane(pane_id) else {
        return;
    };
    if pane.phase != super::workspace_runtime::RuntimePanePhase::Ready
        || pane.session_id.as_ref() != Some(session_id)
        || !pane.open_sftp_after_connect
    {
        return;
    }
    let Some(SessionTarget::Ssh(ssh)) = pane.target.as_ref() else {
        return;
    };
    let host_id = ssh.host_id.clone();

    match root.workspace_runtime.take_open_sftp_after_connect(pane_id) {
        Ok(true) => {}
        Ok(false) | Err(_) => {
            root.status = Some(crate::locale::text(
                "终端已连接，但自动 SFTP 意图无法确认已消费。",
                "The terminal connected, but its one-shot SFTP intent could not be consumed.",
            ));
            cx.notify();
            return;
        }
    }

    let Some(files) = root.files_pane.clone() else {
        root.status = Some(crate::locale::text(
            "终端已连接，但文件服务不可用，无法打开 SFTP。",
            "The terminal connected, but file services are unavailable for SFTP.",
        ));
        cx.notify();
        return;
    };
    let Some(lease) = root
        .workspace_runtime
        .pane(pane_id)
        .and_then(|pane| pane.external_lease.as_ref())
        .and_then(|lease| lease.retain().ok())
    else {
        root.status = Some(crate::locale::text(
            "终端已连接，但无法为 SFTP 保留独立连接租约；可手动打开 SFTP。",
            "The terminal connected, but an independent SFTP lease could not be retained; open SFTP manually.",
        ));
        cx.notify();
        return;
    };
    let path = match RemotePath::parse("/") {
        Ok(path) => path,
        Err(_) => {
            root.status = Some(crate::locale::text(
                "终端已连接，但 SFTP 起始路径无效。",
                "The terminal connected, but its SFTP start path is invalid.",
            ));
            cx.notify();
            return;
        }
    };
    let endpoint = FileEndpoint::Remote { host_id, path };
    match files.update(cx, |pane, cx| {
        pane.open_external_endpoint(endpoint, &lease, cx)
    }) {
        Ok(()) => {
            root.open_shared_sftp_workbench(window, cx);
            root.status = None;
        }
        Err(error) => {
            root.status = Some(crate::locale::text(
                error.public_message(),
                "The terminal connected, but its automatic SFTP workspace could not open.",
            ));
        }
    }
    cx.notify();
}
/// 在连接面板展示外部终端的用户可识别名称；无别名时使用规范化 endpoint 主机名。
fn external_terminal_title(target: &ExternalRoutedTarget) -> String {
    target
        .display_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| target.identity.endpoint.host.clone())
}

/// 建立不含凭据、argv 或工作目录的持久化 pane 意图；这些内容只留在运行态目标中。
fn external_ssh_workspace_pane(host_id: &HostId, title: &str) -> WorkspaceSessionPane {
    WorkspaceSessionPane {
        id: PaneId::new(),
        title: title.to_owned(),
        machine_id: host_id.as_str().to_owned(),
        mode: WorkspacePaneMode::Ssh,
        target: Some(WorkspaceTargetRef::Ssh {
            host_id: host_id.clone(),
        }),
        remote_host_id: Some(host_id.clone()),
        container_id: None,
        profile_id: None,
        shell: None,
        args: None,
        cwd: None,
        current_cwd: None,
        env: None,
        remote_command: None,
        tmux_binding: None,
        prompt: String::new(),
        status: WorkspaceMachineStatus::Offline,
        latency_ms: None,
        local_machine_scope: None,
        output_history: None,
    }
}

/// 只有创建会话尚未交给 session owner 时才取消代次并移除空运行态占位。
fn discard_external_reservation(
    root: &mut RootView,
    pane_id: &PaneId,
    cancellation: &tokio_util::sync::CancellationToken,
    cx: &mut Context<RootView>,
) {
    cancellation.cancel();
    root.cancel_workspace_restore(pane_id);
    let _ = root.workspace_runtime.remove_empty_pane(pane_id);
    root.refresh_workspace_views(cx);
}

/// 新建标签保存精确远端身份和左右起始目录；本地路径不可序列化时不写入不完整快照。
///
/// @author kongweiguang
fn create_sftp_transfer_workspace_tab(
    root: &mut RootView,
    endpoint: &FileEndpoint,
    title: &str,
    initial_local_directory: Option<&std::path::Path>,
    should_activate: bool,
    window: &mut Window,
    cx: &mut Context<RootView>,
) -> bool {
    let FileEndpoint::Remote { host_id, path } = endpoint else {
        root.status = Some(crate::locale::text(
            "传输标签需要明确的远端主机。",
            "A transfer tab requires an explicit remote host.",
        ));
        cx.notify();
        return false;
    };
    if host_id.as_str().starts_with("external:") {
        root.status = Some(crate::locale::text(
            "临时外部连接不能保存为可恢复的传输标签。",
            "Temporary external targets cannot be saved as restorable transfer tabs.",
        ));
        cx.notify();
        return false;
    }
    if root.host_shutdown_requested || root.workspace_load_pending {
        root.status = Some(crate::locale::text(
            "工作区正在关闭或读取，暂时无法创建传输标签。",
            "The workspace is loading or shutting down; a transfer tab cannot be created yet.",
        ));
        cx.notify();
        return false;
    }
    let Some(controller) = root.workspace_controller.as_ref() else {
        root.status = Some(crate::locale::text(
            "工作区处于只读恢复状态，无法创建传输标签。",
            "The workspace is in read-only recovery and cannot create a transfer tab.",
        ));
        cx.notify();
        return false;
    };
    if controller.pending_resource_operation().is_some()
        || matches!(
            controller.persistence_state(),
            WorkspacePersistenceState::Conflict { .. }
                | WorkspacePersistenceState::OutcomeUnknown { .. }
                | WorkspacePersistenceState::ReadbackConfirmedUncommitted { .. }
        )
    {
        root.status = Some(crate::locale::text(
            "工作区有未完成的资源或保存冲突，请先处理后再创建传输标签。",
            "Resolve the pending workspace resource or save conflict before creating a transfer tab.",
        ));
        cx.notify();
        return false;
    }

    let initial_left_path = match initial_local_directory {
        Some(path) => match path.to_str() {
            Some(path) if !path.trim().is_empty() => Some(path.to_owned()),
            Some(_) => None,
            None => {
                root.status = Some(crate::locale::text(
                    "主机的本地起始目录无法保存到工作区标签。",
                    "The host's local start directory cannot be stored in the workspace tab.",
                ));
                cx.notify();
                return false;
            }
        },
        None => None,
    };
    let tab_id = TabId::new();
    let tab = sftp_transfer_tab_for_host(
        tab_id.clone(),
        host_id.clone(),
        initial_left_path,
        path,
        title,
    );
    let mut snapshot = controller.snapshot().clone();
    if snapshot
        .terminal_tabs
        .iter()
        .any(|existing| existing.id() == &tab_id)
    {
        root.status = Some(crate::locale::text(
            "工作区标签身份冲突，未创建传输标签。",
            "A workspace tab identity conflicted; no transfer tab was created.",
        ));
        cx.notify();
        return false;
    }
    snapshot.terminal_tabs.push(tab.clone());
    if should_activate {
        snapshot.active_tab_id = Some(tab_id.clone());
        snapshot.focused_pane_id = None;
        snapshot.selected_machine_id = Some(host_id.as_str().to_owned());
    }

    if !root.workspace_files.register_pending_sftp_transfer_tab(tab) {
        root.status = Some(crate::locale::text(
            "文件工作区尚未就绪或标签身份已存在，未创建传输标签。",
            "The file workspace is not ready or the tab already exists; no transfer tab was created.",
        ));
        cx.notify();
        return false;
    }
    if let Err(error) = root
        .workspace_controller
        .as_mut()
        .expect("controller was checked above")
        .synchronize_snapshot(snapshot)
    {
        root.remove_workspace_file_tab_after_close(&tab_id);
        root.status = Some(error.public_message().into());
        cx.notify();
        return false;
    }

    root.refresh_workspace_views(cx);
    if should_activate {
        root.activate_workspace_tab(&tab_id, window, cx);
    }
    root.persist_workspace_snapshot(cx);
    true
}
/// 仅当连接页仍是当前中心页时恢复重名前的工作区，保留显式导航优先级。
///
/// @author kongweiguang
fn group_rename_return_target(
    current_panel: CenterPanel,
    return_panel: Option<CenterPanel>,
) -> Option<CenterPanel> {
    if current_panel == CenterPanel::Connections {
        return_panel
    } else {
        None
    }
}

/// 消费保存/取消的分组编辑完成结果；迟到回执不会覆盖随后切换的工作区。
/// 返回属于临时导航恢复，保留原文件展示来源，不将独立传输页当成重新打开共享工作台。
///
/// @author kongweiguang
fn finish_group_rename_return(
    root: &mut RootView,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    let return_panel = root.group_rename_return_surface.take();
    let Some(return_panel) = group_rename_return_target(root.center_panel, return_panel) else {
        return;
    };
    if root.center_panel != return_panel {
        let files_selection = root.files_workspace_selection.clone();
        root.set_center_panel(return_panel, window, cx);
        if return_panel == CenterPanel::Files {
            root.files_workspace_selection = files_selection;
        }
    }
}

/// 路由主机与本地 Profile 意图；容器目标由活动终端绑定，不从左栏选择或覆盖。
///
/// Hosts 选择切换到中心详情；创建/重命名共享原工作区返回点，中心不再打开重复连接目录。
/// 本地主机表单复用 ConnectionsPanel 的编辑器模态；连接与传输继续使用原有会话路径。
/// 顶部工作台和折叠直接路由现有 Root 状态，不要求选中主机或先创建终端。
/// @author kongweiguang
pub(super) fn handle_host_navigation_event(
    root: &mut RootView,
    event: &HostNavigationEvent,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    let group_editor_return_panel = if matches!(
        event,
        HostNavigationEvent::CreateGroupRequested
            | HostNavigationEvent::RenameGroupRequested { .. }
    ) {
        Some(
            root.group_rename_return_surface
                .unwrap_or(root.center_panel),
        )
    } else {
        root.group_rename_return_surface = None;
        None
    };
    if group_editor_return_panel.is_some() && root.group_rename_return_surface.is_some() {
        // 忽略编辑中重复触发，避免重置分组草稿或覆盖原工作区返回点。
        return;
    }
    match event {
        HostNavigationEvent::OpenSftpWorkbenchRequested => {
            if root.files_pane.is_none() {
                root.status = Some(crate::locale::text(
                    "文件服务尚未配置，无法打开 SFTP 工作台。",
                    "File services are unavailable, so the SFTP workbench cannot be opened.",
                ));
                cx.notify();
                return;
            }
            if root.compact_host_drawer_open {
                root.close_compact_drawer(window, cx);
            }
            root.open_shared_sftp_workbench(window, cx);
            return;
        }
        HostNavigationEvent::LocalProfileConnectRequested { profile_id } => {
            root.create_terminal_for_target(local_profile_target_request(profile_id), window, cx);
            return;
        }
        HostNavigationEvent::LocalProfileSelected { profile_id } => {
            // 点击本地行显示同一 Profile 的中心详情，并清除上一次远端选择以免运维目标漂移。
            if let Some(panel) = root.connections_panel.clone() {
                panel.update(cx, |panel, cx| {
                    panel.clear_host_selection(cx);
                    panel.select_local_host(Some(profile_id.clone()), window, cx);
                });
            }
            root.selected_host_id = None;
            root.refresh_operations_panel_targets(cx);
            root.set_center_panel(CenterPanel::Connections, window, cx);
            cx.notify();
            return;
        }
        HostNavigationEvent::HostSelected { .. } => {
            root.set_center_panel(CenterPanel::Connections, window, cx);
        }
        HostNavigationEvent::ConnectRequested { host_id } => {
            match existing_host_session_target(root, host_id) {
                ExistingHostSessionTarget::Unique { tab_id, pane_id } => {
                    activate_host_session_target(root, &tab_id, &pane_id, window, cx);
                    return;
                }
                ExistingHostSessionTarget::Multiple => {
                    root.status = Some(crate::locale::text(
                        "此主机有多个活动会话，请在工作区中选择目标标签和分屏。",
                        "This host has multiple active sessions. Choose the intended tab and pane in the workspace.",
                    ));
                    cx.notify();
                    return;
                }
                ExistingHostSessionTarget::Missing => {}
            }
        }
        HostNavigationEvent::ActivateExistingSessionRequested { host_id } => {
            match existing_host_session_target(root, host_id) {
                ExistingHostSessionTarget::Unique { tab_id, pane_id } => {
                    activate_host_session_target(root, &tab_id, &pane_id, window, cx);
                }
                ExistingHostSessionTarget::Multiple => {
                    root.status = Some(crate::locale::text(
                        "此主机有多个活动会话，请在工作区中选择目标标签和分屏。",
                        "This host has multiple active sessions. Choose the intended tab and pane in the workspace.",
                    ));
                    cx.notify();
                }
                ExistingHostSessionTarget::Missing => {
                    root.status = Some(crate::locale::text(
                        "活动会话状态已变化，未新建连接；请重新查看主机状态。",
                        "The active session changed. No new connection was created; review the host status and try again.",
                    ));
                    cx.notify();
                }
            }
            return;
        }
        HostNavigationEvent::OpenSftpRequested { .. } => {}
        HostNavigationEvent::CreateSftpTransferTabRequested { .. } => {}
        HostNavigationEvent::EditHostRequested { .. }
        | HostNavigationEvent::CopyHostRequested { .. }
        | HostNavigationEvent::CreateHostInGroupRequested { .. }
        | HostNavigationEvent::CreateLocalProfileRequested { .. }
        | HostNavigationEvent::EditLocalProfileRequested { .. }
        | HostNavigationEvent::CopyLocalProfileRequested { .. } => {
            // 编辑草稿只覆盖 Root 当前工作区；缺服务时继续落到下方可见错误分支。
            if root.connections_panel.is_some() && !root.prepare_connection_editor_modal(window, cx)
            {
                return;
            }
        }
        HostNavigationEvent::ShowHostDetailsRequested { .. }
        | HostNavigationEvent::DeleteHostRequested { .. }
        | HostNavigationEvent::DeleteLocalProfileRequested { .. }
        | HostNavigationEvent::CreateGroupRequested
        | HostNavigationEvent::RenameGroupRequested { .. }
        | HostNavigationEvent::DeleteGroupRequested { .. } => {
            root.set_center_panel(CenterPanel::Connections, window, cx);
        }
    }

    let Some(panel) = root.connections_panel.clone() else {
        root.status = Some(crate::locale::text(
            "主机连接服务尚未配置。",
            "Host connection services are unavailable.",
        ));
        if let Some(return_panel) = group_editor_return_panel
            && root.center_panel != return_panel
        {
            root.set_center_panel(return_panel, window, cx);
        }
        cx.notify();
        return;
    };

    let mut group_editor_started = false;
    let mut group_editor_failed = false;
    panel.update(cx, |panel, cx| match event {
        HostNavigationEvent::OpenSftpWorkbenchRequested
        | HostNavigationEvent::LocalProfileSelected { .. }
        | HostNavigationEvent::LocalProfileConnectRequested { .. }
        | HostNavigationEvent::ActivateExistingSessionRequested { .. } => {}
        HostNavigationEvent::HostSelected { host_id } => panel.select_host_id(host_id, cx),
        HostNavigationEvent::ConnectRequested { host_id } => {
            panel.request_connect_host(host_id, cx)
        }
        HostNavigationEvent::OpenSftpRequested { host_id } => {
            panel.request_open_sftp_host(host_id, cx)
        }
        HostNavigationEvent::CreateSftpTransferTabRequested { host_id } => {
            panel.request_create_sftp_transfer_tab_host(host_id, cx)
        }
        HostNavigationEvent::ShowHostDetailsRequested { host_id } => {
            panel.show_saved_host(host_id, cx)
        }
        HostNavigationEvent::EditHostRequested { host_id } => {
            panel.edit_saved_host(host_id, window, cx)
        }
        HostNavigationEvent::CopyHostRequested { host_id } => {
            panel.duplicate_saved_host(host_id, window, cx)
        }
        HostNavigationEvent::DeleteHostRequested { host_id } => {
            panel.request_delete_host(host_id, cx)
        }
        HostNavigationEvent::CreateHostInGroupRequested { group_id } => {
            panel.create_host_in_group(group_id, window, cx)
        }
        HostNavigationEvent::CreateLocalProfileRequested { group_id } => {
            panel.begin_create_local_host(group_id.clone(), window, cx)
        }
        HostNavigationEvent::EditLocalProfileRequested { profile_id } => {
            panel.begin_edit_local_host(profile_id.clone(), window, cx)
        }
        HostNavigationEvent::CopyLocalProfileRequested { profile_id } => {
            panel.begin_duplicate_local_host(profile_id.clone(), window, cx)
        }
        HostNavigationEvent::DeleteLocalProfileRequested { profile_id } => {
            panel.request_delete_local_host(profile_id.clone(), window, cx)
        }
        HostNavigationEvent::CreateGroupRequested => {
            group_editor_started = panel.begin_create_group(window, cx);
            group_editor_failed = !group_editor_started;
        }
        HostNavigationEvent::RenameGroupRequested { group_id } => {
            group_editor_started = panel.begin_rename_group(group_id, window, cx);
            group_editor_failed = !group_editor_started;
        }
        HostNavigationEvent::DeleteGroupRequested { group_id } => {
            panel.request_delete_group(group_id, cx)
        }
    });
    if let Some(return_panel) = group_editor_return_panel {
        if group_editor_started {
            root.group_rename_return_surface = Some(return_panel);
        } else if root.center_panel != return_panel {
            root.set_center_panel(return_panel, window, cx);
        }
    }
    if group_editor_failed {
        root.status = Some(crate::locale::text(
            "分组编辑器未打开。请检查只读恢复状态、未完成的编辑或删除操作后重试。",
            "The group editor could not be opened. Check recovery mode and finish pending edits or deletions before retrying.",
        ));
        cx.notify();
    }
}

/// 将本地主机连接意图保留为 Profile 目标，防止统一列表导航时重解释其身份。
///
/// @author kongweiguang
fn local_profile_target_request(profile_id: &str) -> TerminalTargetRequest {
    TerminalTargetRequest::ProfileLocal {
        profile_id: profile_id.to_owned(),
    }
}

/// 只从当前有效 pane 中选取明确属于该 HostId 的 Ready/Opening 会话，拒绝旧投影和目标漂移。
/// @author kongweiguang
fn existing_host_session_target(
    root: &RootView,
    requested_host_id: &HostId,
) -> ExistingHostSessionTarget {
    let Some(controller) = root.workspace_controller.as_ref() else {
        return ExistingHostSessionTarget::Missing;
    };
    let snapshot = controller.snapshot();
    let candidates = root
        .workspace_runtime
        .panes()
        .filter_map(|(pane_id, pane)| {
            if !matches!(
                session_status_for_phase(pane.phase),
                Some(HostSessionStatus::Ready | HostSessionStatus::Opening)
            ) {
                return None;
            }
            let (host_id, protocol) = persisted_remote_host(&pane.persisted.target)?;
            if &host_id != requested_host_id
                || pane
                    .persisted
                    .remote_host_id
                    .as_ref()
                    .is_some_and(|persisted_id| persisted_id != &host_id)
            {
                return None;
            }
            if let Some(target) = pane.target.as_ref() {
                if !runtime_target_matches(&host_id, protocol, target) {
                    return None;
                }
            } else if pane.phase == RuntimePanePhase::Ready || pane.session_id.is_some() {
                return None;
            }
            if pane.phase == RuntimePanePhase::Ready && pane.session_id.is_none() {
                return None;
            }
            let tab_contains_pane = snapshot.terminal_tabs.iter().any(|tab| {
                tab.id() == &pane.tab_id
                    && tab
                        .terminal_layout()
                        .is_some_and(|layout| layout.contains_pane(pane_id))
            });
            tab_contains_pane.then(|| (pane.tab_id.clone(), pane_id.clone()))
        })
        .collect();
    select_existing_host_session_target(candidates)
}

/// 自动激活只接受唯一稳定目标；多个会话必须由用户在工作区自行选择。
/// @author kongweiguang
fn select_existing_host_session_target(
    candidates: Vec<(TabId, PaneId)>,
) -> ExistingHostSessionTarget {
    match candidates.as_slice() {
        [] => ExistingHostSessionTarget::Missing,
        [(tab_id, pane_id)] => ExistingHostSessionTarget::Unique {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        },
        _ => ExistingHostSessionTarget::Multiple,
    }
}

/// 复用 Root 已有的标签和 pane 激活路径，确保点击只切换视图，不新建或重绑 session。
/// @author kongweiguang
fn activate_host_session_target(
    root: &mut RootView,
    tab_id: &TabId,
    pane_id: &PaneId,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    root.activate_workspace_tab(tab_id, window, cx);
    root.focus_workspace_pane(tab_id.clone(), pane_id.clone(), window, cx);
}

#[derive(Debug, Eq, PartialEq)]
enum ExistingHostSessionTarget {
    Missing,
    Unique { tab_id: TabId, pane_id: PaneId },
    Multiple,
}

/// 从真实 workspace pane 汇总主机行状态，不用活动焦点或网络探测推断连接状态。
/// @author kongweiguang
pub(super) fn refresh_host_session_projection(root: &mut RootView, cx: &mut Context<RootView>) {
    let Some(navigation) = root.host_navigation.clone() else {
        return;
    };

    let projection = root
        .workspace_runtime
        .panes()
        .filter_map(|(_, pane)| {
            let (host_id, protocol) = persisted_remote_host(&pane.persisted.target)?;
            if pane
                .persisted
                .remote_host_id
                .as_ref()
                .is_some_and(|persisted_id| persisted_id != &host_id)
            {
                return None;
            }
            if let Some(target) = pane.target.as_ref() {
                if !runtime_target_matches(&host_id, protocol, target) {
                    return None;
                }
            } else if pane.phase == RuntimePanePhase::Ready {
                return None;
            }

            Some(HostSessionProjection {
                host_id,
                status: session_status_for_phase(pane.phase)?,
            })
        })
        .collect();
    navigation.update(cx, |navigation, cx| {
        navigation.set_session_projection(projection, cx)
    });
}

/// 仅把持久目标中显式的远程主机身份用于导航，不从 machine 名称猜 HostId。
/// @author kongweiguang
fn persisted_remote_host(target: &Option<WorkspaceTargetRef>) -> Option<(HostId, RemoteProtocol)> {
    match target.as_ref()? {
        WorkspaceTargetRef::Ssh { host_id } => Some((host_id.clone(), RemoteProtocol::Ssh)),
        WorkspaceTargetRef::Telnet { host_id } => Some((host_id.clone(), RemoteProtocol::Telnet)),
        WorkspaceTargetRef::Serial { host_id } => Some((host_id.clone(), RemoteProtocol::Serial)),
        WorkspaceTargetRef::Local { .. }
        | WorkspaceTargetRef::DockerContainer { .. }
        | WorkspaceTargetRef::Unavailable { .. } => None,
    }
}

/// 运行目标若存在必须与持久 pane 的协议/主机一致；串口 target 不含 HostId，只核对协议类型。
/// @author kongweiguang
fn runtime_target_matches(
    host_id: &HostId,
    protocol: RemoteProtocol,
    target: &SessionTarget,
) -> bool {
    match (protocol, target) {
        (RemoteProtocol::Ssh, SessionTarget::Ssh(target)) => &target.host_id == host_id,
        // Telnet 和 Serial 的 transport target 不携带 HostId；关联身份由同 pane 的持久目标确定。
        (RemoteProtocol::Telnet, SessionTarget::Telnet(_)) => true,
        (RemoteProtocol::Serial, SessionTarget::Serial(_)) => true,
        _ => false,
    }
}

/// 折叠运行阶段为可见状态；Closing 不再显示为已连接，失败关闭保留错误标记。
/// @author kongweiguang
fn session_status_for_phase(phase: RuntimePanePhase) -> Option<HostSessionStatus> {
    match phase {
        RuntimePanePhase::RestorePending => Some(HostSessionStatus::Opening),
        RuntimePanePhase::Ready => Some(HostSessionStatus::Ready),
        RuntimePanePhase::RestoreFailed | RuntimePanePhase::CloseFailed => {
            Some(HostSessionStatus::Failed)
        }
        RuntimePanePhase::Closing => None,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_connections.rs"]
mod tests;
