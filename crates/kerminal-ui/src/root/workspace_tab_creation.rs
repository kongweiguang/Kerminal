//! 标签栏的新建入口复用已有连接目录与会话创建流程。
//!
//! @author kongweiguang

use super::*;
use kerminal_domain::connections::RemoteProtocol;

impl RootView {
    /// 菜单只携带稳定目标，执行前重查当前目录；新建入口始终创建新会话，不借用现存主机 Tab。
    /// 本地 Profile 交给统一 resolver，远程主机继续经过既有连接预检和信任流程。
    ///
    /// @author kongweiguang
    pub(super) fn handle_workspace_tabs_create_event(
        &mut self,
        event: &WorkspaceTabsCreateEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.host_shutdown_requested
            || self.pending_quit_intent.is_some()
            || self.quit_ready_intent.is_some()
            || window.has_active_prompt()
            || self.preferences_modal_open
            || self.connections_modal_open
            || self.connection_prompt_modal_open
            || self.snippet_editor_modal_open
            || self.root_confirmation_owner.is_some()
        {
            return;
        }
        if self.split_target_picker.read(cx).is_open() {
            let return_focus = self
                .split_target_picker
                .update(cx, |picker, cx| picker.close(window, cx));
            if let Some(focus) = return_focus {
                window.focus(&focus, cx);
            }
        }
        if self.is_creating_session {
            self.status = Some(crate::locale::text(
                "正在创建终端，请稍后重试。",
                "A terminal is being created. Try again when it finishes.",
            ));
            cx.notify();
            return;
        }
        let WorkspaceTabsCreateEvent::Target(target) = event else {
            self.create_terminal_for_target(TerminalTargetRequest::ProfileDefaultLocal, window, cx);
            return;
        };
        let available = self.host_navigation.as_ref().is_some_and(|navigation| {
            let navigation = navigation.read(cx);
            !navigation.split_target_loading()
                && navigation
                    .split_target_options("")
                    .iter()
                    .any(|option| &option.target == target)
        });
        if !available {
            self.status = Some(crate::locale::text(
                "主机目录已变化，请重新打开新建终端菜单。",
                "The host directory changed. Reopen the new terminal menu.",
            ));
            cx.notify();
            return;
        }
        match target {
            WorkspaceTargetRef::Local { profile_id } => {
                let request = profile_id.as_ref().map_or(
                    TerminalTargetRequest::ProfileDefaultLocal,
                    |profile_id| TerminalTargetRequest::ProfileLocal {
                        profile_id: profile_id.clone(),
                    },
                );
                self.create_terminal_for_target(request, window, cx);
            }
            WorkspaceTargetRef::Ssh { host_id }
            | WorkspaceTargetRef::Telnet { host_id }
            | WorkspaceTargetRef::Serial { host_id } => {
                if let Some(panel) = self.connections_panel.clone() {
                    let protocol = if matches!(target, WorkspaceTargetRef::Ssh { .. }) {
                        RemoteProtocol::Ssh
                    } else if matches!(target, WorkspaceTargetRef::Telnet { .. }) {
                        RemoteProtocol::Telnet
                    } else {
                        RemoteProtocol::Serial
                    };
                    let accepted = panel.update(cx, |panel, cx| {
                        panel.request_connect_host_for_protocol(host_id, protocol, cx)
                    });
                    if !accepted {
                        self.status = Some(crate::locale::text(
                            "主机协议或目录已变化，请重新选择目标。",
                            "The host protocol or directory changed. Select the target again.",
                        ));
                        cx.notify();
                    }
                } else {
                    self.status = Some(crate::locale::text(
                        "主机连接服务尚未就绪。",
                        "Host connection services are not ready.",
                    ));
                    cx.notify();
                }
            }
            WorkspaceTargetRef::DockerContainer { .. } | WorkspaceTargetRef::Unavailable { .. } => {
                self.status = Some(crate::locale::text(
                    "此目标不能从主机菜单新建终端。",
                    "This target cannot be opened from the host menu.",
                ));
                cx.notify();
            }
        }
    }
}
