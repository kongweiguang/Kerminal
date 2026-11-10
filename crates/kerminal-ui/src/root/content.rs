//! 中央内容的宿主渲染边界；导航和业务资源仍由原来的 owner 管理。
//!
//! @author kongweiguang

use super::*;

impl RootView {
    /// 变更动作携带宿主分配的实例代次；后台回执也只能保存它原有标签的状态。
    /// 保存继续进入既有 CAS 与结果未知门禁，不因当前活动页面变化而重新绑定目标。
    ///
    /// @author kongweiguang
    pub(super) fn plugin_content_state_changed(
        &mut self,
        action: &crate::plugins::content::ContentStateChanged,
        cx: &mut Context<Self>,
    ) {
        use crate::plugins::content::ContentScope;
        if self.host_shutdown_requested || !self.content_host.matches_identity(&action.identity) {
            return;
        }
        let ContentScope::Tab(tab_id) = action.identity.scope() else {
            return;
        };
        let result = self
            .content_host
            .snapshot(action.identity.scope(), cx)
            .and_then(|state| {
                self.workspace_controller
                    .as_mut()
                    .ok_or_else(|| kerminal_application::AppError::conflict("工作区尚未就绪"))?
                    .update_plugin_tab_state(tab_id, state.version, state.value)
            });
        match result {
            Ok(true) => self.persist_workspace_snapshot(cx),
            Ok(false) => {}
            Err(error) => {
                self.content_failures.insert(
                    action.identity.scope().clone(),
                    error.public_message().to_owned().into(),
                );
                self.status = Some(error.public_message().to_owned().into());
                cx.notify();
            }
        }
    }

    /// 初始加载只有数据上下文，视图工厂延至所属窗口生命周期执行；导航换代后放弃旧恢复。
    /// 不聚焦后台窗口，也不为隐藏标签创建实例或启动网络读取。
    ///
    /// @author kongweiguang
    pub(super) fn restore_active_plugin_tab(&mut self, cx: &mut Context<Self>) {
        if self.center_panel != CenterPanel::Contributed {
            return;
        }
        let tab_id = self.workspace_controller.as_ref().and_then(|controller| {
            let snapshot = controller.snapshot();
            snapshot.active_tab_id.as_ref().and_then(|id| {
                snapshot
                    .terminal_tabs
                    .iter()
                    .find(|tab| tab.id() == id)
                    .filter(|tab| matches!(tab, WorkspaceSessionTab::Extension { .. }))
                    .map(|tab| tab.id().clone())
            })
        });
        let Some(tab_id) = tab_id else {
            return;
        };
        self.active_content_scope =
            Some(crate::plugins::content::ContentScope::Tab(tab_id.clone()));
        let generation = self.surface_navigation_generation;
        let window_handle = self.window_handle;
        cx.spawn(async move |this, cx| {
            let _ = cx.update_window(window_handle, move |_, window, app| {
                let _ = this.update(app, |root, cx| {
                    if root.center_panel == CenterPanel::Contributed
                        && root.surface_navigation_generation == generation
                        && !root.host_shutdown_requested
                        && root
                            .workspace_controller
                            .as_ref()
                            .is_some_and(|controller| {
                                controller.snapshot().active_tab_id.as_ref() == Some(&tab_id)
                            })
                    {
                        root.activate_workspace_tab_surface(&tab_id, false, window, cx);
                    }
                });
            });
        })
        .detach();
    }

    /// 新内容先进入工作区的同一校验与保存门禁，再按标签身份激活工厂。
    /// 恢复失败仍留下可见标签和原始状态，允许用户重试或显式关闭。
    ///
    /// @author kongweiguang
    pub fn open_plugin_tab(
        &mut self,
        request: &crate::plugins::actions::OpenContentTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<TabId> {
        if self.host_shutdown_requested || self.tool_panel_overlay_focus_is_blocked(window, cx) {
            return Err(kerminal_application::AppError::conflict(
                "请先完成当前操作再打开标签",
            ));
        }
        if self
            .services
            .plugins
            .content_type(&request.content_id)
            .is_none()
        {
            return Err(kerminal_application::AppError::not_found(
                "内容提供方不可用",
            ));
        }
        self.capture_plugin_tab_states(cx)?;
        let controller = self
            .workspace_controller
            .as_mut()
            .ok_or_else(|| kerminal_application::AppError::conflict("工作区尚未就绪"))?;
        let tab_id = TabId::new();
        let mut snapshot = controller.snapshot().clone();
        snapshot.terminal_tabs.push(WorkspaceSessionTab::Extension {
            id: tab_id.clone(),
            tab_group_id: None,
            title: request.title.clone(),
            content_id: request.content_id.clone(),
            state_version: request.state_version,
            state: request.state.clone(),
            extra_fields: serde_json::Map::new(),
        });
        snapshot.active_tab_id = Some(tab_id.clone());
        snapshot.focused_pane_id = None;
        snapshot.selected_machine_id = None;
        controller.synchronize_snapshot(snapshot)?;
        self.activate_workspace_tab_surface(&tab_id, true, window, cx);
        self.persist_workspace_snapshot(cx);
        Ok(tab_id)
    }

    /// 复用工作区的关闭候选与批次结果；内容资源回收成功之前不删除标签。
    /// 所有阶段共用同一绝对截止时间，超时或迟到失败仍由原实例保留清理所有权。
    ///
    /// @author kongweiguang
    pub(super) fn begin_plugin_tab_close(
        &mut self,
        operation_id: String,
        tab_id: TabId,
        cx: &mut Context<Self>,
    ) {
        use crate::plugins::content::{ContentPhase, ContentScope};
        let scope = ContentScope::Tab(tab_id.clone());
        match self.content_host.phase(&scope) {
            None | Some(ContentPhase::Closed) => {
                self.complete_nonterminal_workspace_tab_close(operation_id, tab_id, cx);
                return;
            }
            Some(ContentPhase::Closing) => {
                self.complete_workspace_resource_failure(
                    &operation_id,
                    "内容实例仍在关闭，请等待原操作完成。".into(),
                    cx,
                );
                return;
            }
            Some(ContentPhase::Open | ContentPhase::CloseFailed) => {}
        }
        let deadline = self.plugin_tab_close_deadline(&tab_id);
        let (token, close) = match self.content_host.begin_close(&scope, deadline, cx) {
            Ok(close) => close,
            Err(error) => {
                self.complete_workspace_resource_failure(
                    &operation_id,
                    error.public_message().to_owned().into(),
                    cx,
                );
                return;
            }
        };
        let executor = self.services.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor.run(move |_| async move {
                kerminal_application::executor::await_until(deadline, "关闭内容实例", close).await
            }).await;
            let _ = this.update(cx, |root, cx| {
                // 资源回执仍需记录，但不让旧 controller 操作改写较新的批次或界面反馈。
                let result = root.content_host.complete_close(token, result);
                let current = root.workspace_controller.as_ref()
                    .and_then(|controller| controller.pending_resource_operation())
                    .is_some_and(|operation| matches!(operation,
                        crate::features::workspace_controller::WorkspaceResourceOperation::CloseTab {
                            operation_id: current, tab, ..
                        } if current == &operation_id && tab.id() == &tab_id));
                if current {
                    match result {
                        Ok(()) => root.complete_nonterminal_workspace_tab_close(operation_id, tab_id, cx),
                        Err(error) => root.complete_workspace_resource_failure(
                            &operation_id, error.public_message().to_owned().into(), cx,
                        ),
                    }
                }
            });
        }).detach();
    }

    /// 保存前逐项提取已打开标签的状态；失败立即阻止 CAS，避免用旧快照冒充保存成功。
    /// 从未恢复或已关闭的实例沿用原始持久状态，不触发工厂或网络请求。
    ///
    /// @author kongweiguang
    pub(super) fn capture_plugin_tab_states(
        &mut self,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<()> {
        use crate::plugins::content::{ContentPhase, ContentScope};
        let mut states = Vec::new();
        for scope in self.content_host.scopes() {
            if let ContentScope::Tab(tab_id) = &scope
                && matches!(
                    self.content_host.phase(&scope),
                    Some(ContentPhase::Open | ContentPhase::CloseFailed)
                )
            {
                states.push((tab_id.clone(), self.content_host.snapshot(&scope, cx)?));
            }
        }
        if let Some(controller) = self.workspace_controller.as_mut() {
            for (tab_id, state) in states {
                controller.update_plugin_tab_state(&tab_id, state.version, state.value)?;
            }
        }
        Ok(())
    }

    /// 仅活动标签首次进入时建立实例；切回原标签复用 Entity 和订阅，不重新创建连接。
    /// 创建、激活、显示和焦点均确认后才登记恢复成功；失败时保留原实例并撤销显示。
    ///
    /// @author kongweiguang
    #[allow(clippy::too_many_arguments)]
    pub(super) fn activate_plugin_tab_instance(
        &mut self,
        scope: crate::plugins::content::ContentScope,
        content_id: &str,
        state_version: u32,
        state: serde_json::Value,
        request_focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<()> {
        let result = (|| {
            if self.services.plugins.content_type(content_id).is_none() {
                return Err(kerminal_application::AppError::not_found(
                    "内容提供方不可用",
                ));
            }
            if self.content_host.phase(&scope).is_none() {
                let saved = crate::plugins::content::ContentSavedState {
                    version: state_version,
                    value: state,
                };
                self.content_host
                    .open(scope.clone(), content_id, Some(&saved), window, cx)?;
            } else if self.content_host.content_id(&scope) != Some(content_id) {
                return Err(kerminal_application::AppError::conflict(
                    "内容身份已变化，原实例仍保留",
                ));
            }
            self.content_host.activate(&scope, window, cx)?;
            self.content_host
                .visibility_changed(&scope, true, self.content_window_active, cx)?;
            if request_focus {
                self.content_host.focus(&scope, window, cx)?;
            }
            Ok(())
        })();
        match &result {
            Ok(()) => {
                self.content_failures.remove(&scope);
            }
            Err(error) => {
                self.content_failures
                    .insert(scope.clone(), error.public_message().to_owned().into());
            }
        }
        if let crate::plugins::content::ContentScope::Tab(tab_id) = &scope
            && let Some(controller) = self.workspace_controller.as_mut()
        {
            let restore_result = result.as_ref().map(|_| ()).map_err(|_| {
                crate::features::workspace_controller::WorkspaceRestoreFailure::ResourceFailure
            });
            controller.record_restore_result(
                crate::features::workspace_controller::WorkspaceRestoreItemId::Tab(tab_id.clone()),
                restore_result,
            )?;
        }
        self.sync_plugin_content_visibility(cx);
        result
    }

    /// 内置页面复用既有工作台实体与焦点门禁；扩展页已改由设置弹框持有，不再进入内容宿主。
    /// 文件宿主只接受控制器的当前活动身份；目录无须伪造编辑 endpoint，文档仍核对真实缓冲。
    /// 页面单独确认完整生命周期；失败保留实体与草稿，不能沿用前次就绪状态遮住重试入口。
    ///
    /// @author kongweiguang
    pub fn open_plugin_page(
        &mut self,
        content_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<()> {
        use crate::plugins::builtin_content::{self, BuiltinPageRoute};
        use kerminal_application::AppError;

        if self.host_shutdown_requested || self.tool_panel_overlay_focus_is_blocked(window, cx) {
            return Err(AppError::conflict("请先完成当前操作再打开页面"));
        }
        if self.services.plugins.content_type(content_id).is_none() {
            return Err(AppError::not_found("内容提供方不可用"));
        }
        if let Some(route) = builtin_content::page_route(content_id) {
            match route {
                BuiltinPageRoute::Terminal => {
                    self.set_center_panel(CenterPanel::Terminal, window, cx);
                }
                BuiltinPageRoute::Files => {
                    if !self.open_shared_sftp_workbench(window, cx) {
                        return Err(AppError::conflict("无法打开共享文件工作台"));
                    }
                }
                BuiltinPageRoute::Editor => {
                    if self.file_editor_workspace.is_none() {
                        return Err(AppError::not_found("文件编辑服务尚未配置"));
                    }
                    self.set_center_panel(CenterPanel::Editor, window, cx);
                    // 独立编辑页仍保留原文档导航；文件宿主只使用外层一条标签带。
                    let embedded = self.workspace_controller.as_ref().is_some_and(|controller| {
                        let snapshot = controller.snapshot();
                        snapshot.terminal_tabs.iter().any(|tab| snapshot.active_tab_id.as_ref() == Some(tab.id()) && matches!(tab, kerminal_application::workspace::WorkspaceSessionTab::FileWorkspace { .. }))
                    });
                    if let Some(editor) = self.file_editor_workspace.clone() {
                        editor.update(cx, |editor, cx| {
                            editor.set_embedded_navigation(embedded, cx)
                        });
                    }
                }
                BuiltinPageRoute::Preferences => {
                    self.set_center_panel(CenterPanel::Preferences, window, cx);
                    if !self.preferences_modal_open {
                        return Err(AppError::conflict("设置窗口未能打开"));
                    }
                }
                BuiltinPageRoute::Commands => {
                    self.reconcile_tab_tool_contents(window, cx);
                    if self.commands_view.is_none() {
                        return Err(AppError::not_found("命令片段服务尚未配置"));
                    }
                    self.open_snippets_content(window, cx);
                    if self.center_panel != CenterPanel::Commands
                        && !self.snippets_mounted_in_tool_rail(window, cx)
                    {
                        return Err(AppError::conflict("命令片段页面未能打开"));
                    }
                }
                BuiltinPageRoute::Connections => {
                    if self.connections_panel.is_none() {
                        return Err(AppError::not_found("主机连接服务尚未配置"));
                    }
                    self.set_center_panel(CenterPanel::Connections, window, cx);
                }
                BuiltinPageRoute::Agents => {
                    if self.agents_panel.is_none() {
                        return Err(AppError::not_found("Agent 服务尚未配置"));
                    }
                    let tool = ToolRailToolId::AgentLauncher;
                    self.ensure_tool_panel_visible(tool, window, cx);
                    let settings = self.current_tool_rail_settings();
                    let panels = self.tool_rail_state.project(&settings).panels;
                    if !super::workbench::tool_rail_panel_mounted(self, panels, tool, window, cx) {
                        return Err(AppError::conflict("Agent 工作区未能打开"));
                    }
                }
                BuiltinPageRoute::SftpTransfer | BuiltinPageRoute::WorkspaceFile => {
                    let tab_id = self
                        .workspace_controller
                        .as_ref()
                        .and_then(|controller| {
                            let snapshot = controller.snapshot();
                            let active_id = snapshot.active_tab_id.as_ref()?;
                            snapshot
                                .terminal_tabs
                                .iter()
                                .find(|tab| tab.id() == active_id)
                                .filter(|tab| {
                                    matches!(
                                        (route, tab),
                                        (
                                            BuiltinPageRoute::SftpTransfer,
                                            WorkspaceSessionTab::SftpTransfer { .. }
                                        ) | (
                                            BuiltinPageRoute::WorkspaceFile,
                                            WorkspaceSessionTab::FileWorkspace { .. }
                                                | WorkspaceSessionTab::WorkspaceFile { .. }
                                        )
                                    )
                                })
                                .map(|tab| tab.id().clone())
                        })
                        .ok_or_else(|| AppError::conflict("当前活动标签与请求的页面不匹配"))?;
                    if route == BuiltinPageRoute::WorkspaceFile
                        && self.file_editor_workspace.is_none()
                    {
                        return Err(AppError::not_found("文件编辑服务尚未配置"));
                    }
                    if route == BuiltinPageRoute::SftpTransfer
                        && self.workspace_files.files_pane(&tab_id).is_none()
                        && (self.services.files.is_none() || self.services.transfers.is_none())
                    {
                        return Err(AppError::not_found("文件或传输服务尚未配置"));
                    }
                    self.activate_workspace_tab_surface(&tab_id, true, window, cx);
                    match self.workspace_files.restore_state(&tab_id) {
                        Some(workspace_files::WorkspaceFileRestoreState::Ready) => {}
                        Some(workspace_files::WorkspaceFileRestoreState::Failed { .. }) => {
                            return Err(AppError::conflict("工作区文件标签恢复失败"));
                        }
                        _ => return Err(AppError::conflict("工作区文件标签尚未就绪")),
                    }
                    if route == BuiltinPageRoute::SftpTransfer {
                        if self.workspace_files.files_pane(&tab_id).is_none() {
                            return Err(AppError::conflict("传输标签的文件面板不可用"));
                        }
                    } else {
                        if self.center_panel != CenterPanel::Editor {
                            return Err(AppError::conflict("文件工作区未能显示"));
                        }
                        let requires_document = self
                            .workspace_controller
                            .as_ref()
                            .and_then(|controller| {
                                controller
                                    .snapshot()
                                    .terminal_tabs
                                    .iter()
                                    .find(|tab| tab.id() == &tab_id)
                            })
                            .is_some_and(|tab| match tab {
                                WorkspaceSessionTab::FileWorkspace { state, .. } => matches!(
                                    state.active_item(),
                                    Some(kerminal_application::workspace::FileWorkspaceItem::Document { .. })
                                ),
                                WorkspaceSessionTab::WorkspaceFile { .. } => true,
                                WorkspaceSessionTab::Terminal { .. }
                                | WorkspaceSessionTab::SftpTransfer { .. }
                                | WorkspaceSessionTab::Extension { .. }
                                | WorkspaceSessionTab::Unsupported { .. } => false,
                            });
                        if requires_document {
                            let endpoint = self.workspace_files.file_endpoint(&tab_id);
                            let active_endpoint =
                                self.file_editor_workspace.as_ref().and_then(|editor| {
                                    editor.read(cx).workspace_projection().active_endpoint
                                });
                            if endpoint.is_none() || endpoint != active_endpoint {
                                return Err(AppError::conflict("工作区文件未能激活"));
                            }
                        }
                    }
                }
            }
            return Ok(());
        }
        let scope = crate::plugins::content::ContentScope::Page(content_id.to_owned());
        let result: kerminal_application::AppResult<()> = (|| {
            if self.content_host.phase(&scope).is_none() {
                self.content_host
                    .open(scope.clone(), content_id, None, window, cx)?;
            }
            self.content_host.activate(&scope, window, cx)?;
            self.active_content_scope = Some(scope.clone());
            self.set_center_panel(CenterPanel::Contributed, window, cx);
            self.content_host
                .visibility_changed(&scope, true, self.content_window_active, cx)?;
            self.content_host.focus(&scope, window, cx)?;
            Ok(())
        })();
        match &result {
            Ok(()) => {
                self.ready_content_pages.insert(content_id.to_owned());
                self.content_failures.remove(&scope);
            }
            Err(error) => {
                self.ready_content_pages.remove(content_id);
                self.content_failures
                    .insert(scope, error.public_message().to_owned().into());
            }
        }
        self.sync_plugin_content_visibility(cx);
        cx.notify();
        result
    }

    /// 标签以恢复回执决定挂载，快照保存失败不会误隐藏仍可编辑的草稿。
    /// 页面使用自身生命周期回执，ContentHost 再核对实际窗口的显示确认。
    pub(super) fn plugin_content_is_ready(
        &self,
        scope: &crate::plugins::content::ContentScope,
    ) -> bool {
        use crate::features::workspace_controller::{
            WorkspaceRestoreItemId, WorkspaceRestoreStatus,
        };
        match scope {
            crate::plugins::content::ContentScope::Page(content_id) => {
                self.ready_content_pages.contains(content_id)
            }
            crate::plugins::content::ContentScope::Tab(tab_id) => {
                self.workspace_controller.as_ref().and_then(|controller| {
                    controller.restore_status(&WorkspaceRestoreItemId::Tab(tab_id.clone()))
                }) == Some(WorkspaceRestoreStatus::Ready)
            }
        }
    }

    /// 显隐跟随实际中央挂载和窗口激活，通知失败仍保留实例以便诊断与重试。
    /// 未就绪实例保持隐藏；同步由导航和窗口生命周期触发，不用重绘重试失败激活。
    ///
    /// @author kongweiguang
    pub(super) fn sync_plugin_content_visibility(&mut self, cx: &mut Context<Self>) {
        let visible_scope = (self.center_panel == CenterPanel::Contributed)
            .then(|| self.active_content_scope.clone())
            .flatten();
        for scope in self.content_host.scopes() {
            let visible =
                visible_scope.as_ref() == Some(&scope) && self.plugin_content_is_ready(&scope);
            if let Err(error) = self.content_host.visibility_changed(
                &scope,
                visible,
                self.content_window_active,
                cx,
            ) {
                self.content_failures
                    .insert(scope, error.public_message().to_owned().into());
                self.status = Some(error.public_message().to_owned().into());
            }
        }
    }
}
