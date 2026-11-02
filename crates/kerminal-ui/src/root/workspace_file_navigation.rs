//! 文件导航复用现有编辑缓冲，Root 只协调标签身份、关闭回执和临时连接所有权。
//!
//! @author kongweiguang

use gpui_kit::{AppContext as _, Context, Window};
use kerminal_application::{
    AppError, AppResult,
    platform::external_launch::ExternalTargetLease,
    workspace::{
        WorkspaceContainerRuntime, WorkspaceFileAccess, WorkspaceFileSource, WorkspaceSessionTab,
        WorkspaceTargetRef,
    },
};
use kerminal_domain::{
    files::{ContainerRuntime, FileEndpoint},
    identity::TabId,
};

use crate::features::{
    editor::EditorWorkspaceProjection,
    workspace_controller::WorkspacePersistenceState,
    workspace_tabs::{WorkspaceTabEvent, WorkspaceTabProjection},
};

use super::{CenterPanel, RootView};

impl RootView {
    /// 文件事件冻结来源端点和独立连接 scope；只返回已接受的导航，读取失败仍由目标页展示。
    ///
    /// @author kongweiguang
    pub(super) fn open_file_endpoint_with_lease(
        &mut self,
        endpoint: FileEndpoint,
        external_target_lease: Option<ExternalTargetLease>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.file_editor_workspace.is_none() {
            self.status = Some(crate::locale::text(
                "文件编辑服务尚未配置。",
                "The file editor is unavailable.",
            ));
            cx.notify();
            return false;
        }
        if let Some(id) = self.workspace_files.tab_for_endpoint(&endpoint) {
            if self.handle_workspace_tab_event(&WorkspaceTabEvent::Activate(id.clone()), window, cx)
            {
                self.activate_workspace_tab(&id, window, cx);
                return true;
            }
            return false;
        }
        let result = file_navigation_descriptor(&endpoint, external_target_lease.as_ref());
        let (tab, transient) = match result {
            Ok(value) => value,
            Err(error) => {
                self.status = Some(error.public_message().into());
                cx.notify();
                return false;
            }
        };
        let Some(controller) = self.workspace_controller.as_ref() else {
            self.status = Some(crate::locale::text(
                "工作区尚未就绪，请稍后重试。",
                "The workspace is not ready. Try again shortly.",
            ));
            cx.notify();
            return false;
        };
        let id = tab.id().clone();
        let mut snapshot = controller.snapshot().clone();
        if let WorkspaceSessionTab::WorkspaceFile { machine_id, .. } = &tab {
            snapshot.selected_machine_id = Some(machine_id.clone());
        }
        snapshot.terminal_tabs.push(tab.clone());
        snapshot.active_tab_id = Some(id.clone());
        snapshot.focused_pane_id = None;
        if !self.workspace_files.register_open_file_tab(
            tab,
            endpoint,
            external_target_lease,
            transient,
        ) {
            self.status = Some(crate::locale::text(
                "文件工作区尚未就绪，请重试。",
                "The file workspace is not ready. Try again.",
            ));
            cx.notify();
            return false;
        }
        let result = self
            .workspace_controller
            .as_mut()
            .ok_or_else(|| AppError::conflict("工作区状态已失效"))
            .and_then(|controller| controller.synchronize_snapshot(snapshot));
        if let Err(error) = result {
            self.remove_workspace_file_tab_after_close(&id);
            self.status = Some(error.public_message().into());
            cx.notify();
            return false;
        }
        self.refresh_workspace_views(cx);
        self.activate_workspace_tab(&id, window, cx);
        self.persist_workspace_snapshot(cx);
        true
    }

    /// 内部标签先完成自己的脏稿关闭门禁，Root 再关闭对应导航；后台加载和退出不抢用户导航。
    ///
    /// @author kongweiguang
    pub(super) fn sync_editor_navigation(
        &mut self,
        projection: &EditorWorkspaceProjection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.host_shutdown_requested || self.pending_quit_intent.is_some() {
            self.editor_navigation_sync_pending = true;
            return;
        }
        let resource_pending = self
            .workspace_controller
            .as_ref()
            .is_some_and(|controller| {
                controller.pending_resource_operation().is_some()
                    || matches!(
                        controller.persistence_state(),
                        WorkspacePersistenceState::Conflict { .. }
                            | WorkspacePersistenceState::OutcomeUnknown { .. }
                            | WorkspacePersistenceState::ReadbackConfirmedUncommitted { .. }
                    )
            });
        self.editor_navigation_sync_pending = resource_pending;
        if !resource_pending {
            let active_before_close = self
                .workspace_controller
                .as_ref()
                .and_then(|controller| controller.snapshot().active_tab_id.clone());
            let missing = self
                .workspace_files
                .ready_file_endpoints()
                .into_iter()
                .filter(|(_, endpoint)| {
                    !projection.tabs.iter().any(|tab| &tab.endpoint == endpoint)
                })
                .map(|(id, _)| id)
                .collect::<Vec<_>>();
            let active_was_closed = active_before_close.as_ref().is_some_and(|active| {
                self.nonterminal_workspace_tab_is_visible(active) && missing.contains(active)
            });
            for id in missing {
                self.handle_workspace_tab_event(&WorkspaceTabEvent::Close(id), window, cx);
            }
            if active_was_closed {
                self.activate_current_workspace_surface_after_close(window, cx);
                self.refresh_workspace_views(cx);
                return;
            }
            let active_file_id = self.workspace_controller.as_ref().and_then(|controller| {
                controller
                    .snapshot()
                    .terminal_tabs
                    .iter()
                    .find(|tab| {
                        controller.snapshot().active_tab_id.as_ref() == Some(tab.id())
                            && matches!(tab, WorkspaceSessionTab::WorkspaceFile { .. })
                    })
                    .map(|tab| tab.id().clone())
            });
            if self.center_panel == CenterPanel::Editor
                && let Some(active_id) = active_file_id
                && let Some(endpoint) = projection.active_endpoint.as_ref()
                && self.workspace_files.file_endpoint(&active_id).as_ref() != Some(endpoint)
                && let Some(id) = self.workspace_files.tab_for_endpoint(endpoint)
                && self.handle_workspace_tab_event(
                    &WorkspaceTabEvent::Activate(id.clone()),
                    window,
                    cx,
                )
            {
                self.activate_workspace_tab(&id, window, cx);
            }
        }
        self.refresh_workspace_views(cx);
    }

    /// 关闭后以 reducer 后继同步页面，焦点另等一帧并核对代次和原 owner；模态及新焦点始终优先。
    /// 后台标签关闭不调用此入口，避免刷新或清理抢走当前页面。
    ///
    /// @author kongweiguang
    pub(super) fn activate_current_workspace_surface_after_close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let expected_focus = window.focused(cx);
        let active = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.snapshot().active_tab_id.clone());
        if let Some(id) = active {
            self.activate_workspace_tab_surface(&id, false, window, cx);
            if !self.tool_panel_focus_is_blocked_except_snippet_editor(window, cx, false) {
                let navigation_generation = self.surface_navigation_generation;
                let focus_generation = self.pane_focus_intent_generation;
                let entity = cx.entity();
                window.on_next_frame(move |window, app| {
                    entity.update(app, |root, cx| {
                        if root.surface_navigation_generation == navigation_generation
                            && root.pane_focus_intent_generation == focus_generation
                            && window.focused(cx) == expected_focus
                            && !root.tool_panel_focus_is_blocked_except_snippet_editor(
                                window, cx, false,
                            )
                            && root
                                .workspace_controller
                                .as_ref()
                                .is_some_and(|controller| {
                                    controller.snapshot().active_tab_id.as_ref() == Some(&id)
                                })
                        {
                            root.activate_workspace_tab(&id, window, cx);
                        }
                    });
                });
            }
        } else {
            self.center_panel = CenterPanel::Terminal;
            self.set_reported_focus_session(None, cx);
            self.refresh_workspace_views(cx);
        }
    }

    /// 资源门禁解除后只重读最新编辑器投影，合并等待期间的关闭意图，不重放旧文件或旧目标。
    ///
    /// @author kongweiguang
    pub(super) fn resume_editor_navigation_sync(&mut self, cx: &mut Context<Self>) {
        if !self.editor_navigation_sync_pending
            || self.host_shutdown_requested
            || self.pending_quit_intent.is_some()
            || self
                .workspace_controller
                .as_ref()
                .is_some_and(|controller| {
                    controller.pending_resource_operation().is_some()
                        || matches!(
                            controller.persistence_state(),
                            WorkspacePersistenceState::Conflict { .. }
                                | WorkspacePersistenceState::OutcomeUnknown { .. }
                                | WorkspacePersistenceState::ReadbackConfirmedUncommitted { .. }
                        )
                })
        {
            return;
        }
        self.editor_navigation_sync_pending = false;
        let window_handle = self.window_handle;
        cx.spawn(async move |this, cx| {
            let _ = cx.update_window(window_handle, move |_, window, app| {
                let _ = this.update(app, |root, cx| {
                    if let Some(editor) = root.file_editor_workspace.as_ref() {
                        let projection = editor.read(cx).workspace_projection();
                        root.push_editor_workspace_projection(projection, window, cx);
                    }
                });
            });
        })
        .detach();
    }

    /// 未保存标记仅属于 UI 投影，原文件标题和持久化 descriptor 保持原文。
    ///
    /// @author kongweiguang
    pub(super) fn project_file_dirty_titles(
        &self,
        tabs: &mut [WorkspaceTabProjection],
        cx: &Context<Self>,
    ) {
        let Some(editor) = self.file_editor_workspace.as_ref() else {
            return;
        };
        let projection = editor.read(cx).workspace_projection();
        for tab in tabs {
            if let Some(endpoint) = self.workspace_files.file_endpoint(&tab.id)
                && projection
                    .tabs
                    .iter()
                    .any(|file| file.endpoint == endpoint && file.dirty)
            {
                tab.title = format!("{} •", tab.title).into();
            }
        }
    }
}

/// descriptor 只表达可恢复目标；本机容器使用临时标签，原始端点始终由 runtime 保留。
///
/// @author kongweiguang
fn file_navigation_descriptor(
    endpoint: &FileEndpoint,
    lease: Option<&ExternalTargetLease>,
) -> AppResult<(WorkspaceSessionTab, bool)> {
    let (target, path, machine_id, source, transient) = match endpoint {
        FileEndpoint::Local { path } => (
            WorkspaceTargetRef::Local { profile_id: None },
            path.to_str()
                .ok_or_else(|| AppError::invalid("本机路径不能无损保存为工作区文本"))?
                .to_owned(),
            "local".to_owned(),
            WorkspaceFileSource::Local,
            false,
        ),
        FileEndpoint::Remote { host_id, path } => {
            let transient = host_id.as_str().starts_with("external:");
            if transient && lease.is_none_or(|lease| lease.host_id() != host_id) {
                return Err(AppError::conflict("临时文件目标缺少匹配的连接所有权"));
            }
            (
                WorkspaceTargetRef::Ssh {
                    host_id: host_id.clone(),
                },
                path.to_string(),
                host_id.as_str().to_owned(),
                WorkspaceFileSource::Sftp,
                transient,
            )
        }
        FileEndpoint::Container {
            host_id,
            runtime,
            container_id,
            path,
        } => {
            let transient = host_id.is_none()
                || host_id
                    .as_ref()
                    .is_some_and(|id| id.as_str().starts_with("external:"));
            if let Some(host_id) = host_id
                .as_ref()
                .filter(|id| id.as_str().starts_with("external:"))
                && lease.is_none_or(|lease| lease.host_id() != host_id)
            {
                return Err(AppError::conflict("临时容器文件目标缺少匹配的连接所有权"));
            }
            let target = host_id.as_ref().map_or(
                WorkspaceTargetRef::Local { profile_id: None },
                |host_id| WorkspaceTargetRef::DockerContainer {
                    host_id: host_id.clone(),
                    container_id: container_id.clone(),
                    runtime: match runtime {
                        ContainerRuntime::Docker => WorkspaceContainerRuntime::Docker,
                        ContainerRuntime::Podman => WorkspaceContainerRuntime::Podman,
                    },
                    container_name: None,
                    user: None,
                    workdir: None,
                },
            );
            (
                target,
                path.to_string(),
                format!("container:{container_id}"),
                WorkspaceFileSource::Container,
                transient,
            )
        }
    };
    if path.is_empty() || path.chars().any(char::is_control) {
        return Err(AppError::invalid("文件路径为空或包含控制字符"));
    }
    let title = path
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(&path)
        .to_owned();
    Ok((
        WorkspaceSessionTab::WorkspaceFile {
            id: TabId::new(),
            tab_group_id: None,
            title,
            machine_id,
            target,
            path,
            root_path: None,
            access: WorkspaceFileAccess::Editable,
            source,
        },
        transient,
    ))
}

#[cfg(test)]
#[path = "../../tests/unit/root_workspace_file_navigation.rs"]
mod tests;
