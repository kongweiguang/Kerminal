//! 宿主恢复与目录实体装配；窗口只负责视图，I/O 仍走共享执行器。
//!
//! @author kongweiguang

use super::*;

impl RootView {
    /// 内部缓冲先登记真实身份，后台恢复不能把其活动文档投影成新的顶层导航。
    pub(in crate::root) fn ensure_host_file_workspace(
        &mut self,
        id: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(runtime) = self.workspace_files.tabs.get(id) else {
            return;
        };
        let WorkspaceSessionTab::FileWorkspace {
            target,
            state,
            machine_id,
            ..
        } = runtime.tab.clone()
        else {
            return;
        };
        let Some(anchor) = self.workspace_files.host_workspace_anchor(id) else {
            self.finish_workspace_file_restore(
                id,
                Err(WorkspaceRestoreFailure::ResourceFailure),
                "文件工作区目标无法恢复；原数据已保留。".into(),
                cx,
            );
            return;
        };
        let Some(editor) = self.file_editor_workspace.clone() else {
            self.finish_workspace_file_restore(
                id,
                Err(WorkspaceRestoreFailure::ResourceFailure),
                "文件编辑服务尚未配置。".into(),
                cx,
            );
            return;
        };
        let lease = runtime.external_target_lease.clone();
        let transient = runtime.transient;
        let active = self
            .workspace_controller
            .as_ref()
            .is_some_and(|controller| controller.snapshot().active_tab_id.as_ref() == Some(id));
        for item in &state.items {
            let FileWorkspaceItem::Document {
                id: item_id,
                title,
                path,
                root_path,
                access,
                source,
            } = item
            else {
                continue;
            };
            let endpoint = match self
                .workspace_files
                .host_document_endpoint(id, item_id)
                .ok_or_else(|| AppError::invalid("文档来源或路径与文件工作区目标不匹配"))
            {
                Ok(endpoint) => endpoint,
                Err(error) => {
                    self.finish_workspace_file_restore(
                        id,
                        Err(WorkspaceRestoreFailure::ResourceFailure),
                        error.public_message().into(),
                        cx,
                    );
                    return;
                }
            };
            let readonly = self
                .workspace_files
                .workspace_file_requires_readonly(&endpoint)
                || *access == WorkspaceFileAccess::Readonly;
            let result = editor.update(cx, |workspace, cx| {
                workspace.open_workspace_file(
                    endpoint.clone(),
                    title.clone(),
                    readonly,
                    active && state.active_item_id.as_ref() == Some(item_id),
                    cx,
                )
            });
            if let Err(error) = result {
                self.finish_workspace_file_restore(
                    id,
                    Err(WorkspaceRestoreFailure::ResourceFailure),
                    error.public_message().into(),
                    cx,
                );
                return;
            }
            self.workspace_files.documents.insert(
                (id.clone(), item_id.clone()),
                WorkspaceFileTabRuntime {
                    tab: WorkspaceSessionTab::WorkspaceFile {
                        id: item_id.clone(),
                        tab_group_id: None,
                        title: title.clone(),
                        machine_id: machine_id.clone(),
                        target: target.clone(),
                        path: path.clone(),
                        root_path: root_path.clone(),
                        access: *access,
                        source: *source,
                    },
                    status: WorkspaceFileRestoreState::Ready,
                    endpoint: Some(endpoint),
                    files_pane: None,
                    files_subscriptions: None,
                    external_target_lease: lease.clone(),
                    transient,
                    pending_transfer_endpoints: None,
                },
            );
        }
        self.finish_workspace_file_restore(id, Ok(Some(anchor)), SharedString::default(), cx);
        if active {
            self.activate_host_file_workspace(id, window, cx);
        }
    }

    /// 目录实体按内部稳定 ID 缓存，切文件或重复激活不会再次扫描目录。
    fn ensure_host_directory(
        &mut self,
        owner: &TabId,
        item: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        let key = (owner.clone(), item.clone());
        if self.workspace_files.directories.contains_key(&key) {
            return Ok(());
        }
        let runtime = self
            .workspace_files
            .tabs
            .get(owner)
            .ok_or_else(|| AppError::not_found("文件工作区已关闭"))?;
        let WorkspaceSessionTab::FileWorkspace { state, .. } = &runtime.tab else {
            return Err(AppError::conflict("文件工作区类型已变化"));
        };
        let FileWorkspaceItem::Directory { path, .. } = state
            .items
            .iter()
            .find(|entry| entry.id() == item)
            .ok_or_else(|| AppError::not_found("目录标签已关闭"))?
        else {
            return Err(AppError::invalid("当前标签不是目录"));
        };
        let endpoint = endpoint_at_path(
            &self
                .workspace_files
                .host_workspace_anchor(owner)
                .ok_or_else(|| AppError::conflict("目录目标已失效"))?,
            path,
        )?;
        let lease = runtime.external_target_lease.clone();
        let origins = workspace_origins(owner, state);
        let files = self
            .services
            .files
            .clone()
            .ok_or_else(|| AppError::conflict("文件服务尚未配置"))?;
        let transfers = self
            .services
            .transfers
            .clone()
            .ok_or_else(|| AppError::conflict("传输服务尚未配置"))?;
        let executor = self.services.executor.clone();
        let lookup = crate::features::files::FileTargetNameLookup::new(
            self.services.hosts.clone(),
            self.services.operations.clone(),
        );
        let pane = cx.new(|_| FilesPane::new(files, transfers, executor, lookup, endpoint.clone()));
        let density = super::super::super::preferences::current_interface_density(self);
        pane.update(cx, |pane, cx| {
            pane.set_interface_density(density, cx);
            pane.configure_file_workspace(endpoint, lease, origins, cx)
        })?;
        let subscriptions = Self::subscribe_files_pane_events(&pane, window, cx);
        self.workspace_files
            .directories
            .insert(key, (pane, subscriptions));
        Ok(())
    }

    /// 宿主的目录和文档共用一个中心 surface，但只激活当前内容而不重建另一类状态。
    pub(in crate::root) fn activate_host_file_workspace(
        &mut self,
        id: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(WorkspaceFileTabRuntime {
            tab: WorkspaceSessionTab::FileWorkspace { state, .. },
            ..
        }) = self.workspace_files.tabs.get(id)
        else {
            return;
        };
        let Some(item) = state
            .active_item_id
            .as_ref()
            .and_then(|active| state.items.iter().find(|item| item.id() == active))
            .or_else(|| {
                state
                    .items
                    .iter()
                    .find(|item| matches!(item, FileWorkspaceItem::Directory { .. }))
            })
            .cloned()
        else {
            return;
        };
        if let Some(editor) = self.file_editor_workspace.clone() {
            editor.update(cx, |editor, cx| editor.set_embedded_navigation(true, cx));
        }
        if let FileWorkspaceItem::Directory { id: item_id, .. } = item {
            if let Some(editor) = self.file_editor_workspace.clone() {
                editor.update(cx, |editor, cx| editor.deactivate_document(cx));
            }
            if let Err(error) = self.ensure_host_directory(id, &item_id, window, cx) {
                self.status = Some(error.public_message().into());
            }
        } else if let Some(endpoint) = self.workspace_files.host_active_document(id)
            && let Some(editor) = self.file_editor_workspace.clone()
        {
            editor.update(cx, |editor, cx| {
                editor.activate_open_file(endpoint, cx);
            });
        }
        self.center_panel = CenterPanel::Editor;
        cx.notify();
    }
}
