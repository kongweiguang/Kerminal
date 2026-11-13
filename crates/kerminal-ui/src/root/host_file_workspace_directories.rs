//! 内部目录的打开、关闭和位置回执始终绑定原宿主及原浏览实体。
//!
//! @author kongweiguang

use super::*;

impl RootView {
    /// 显式目录入口才新增目录标签，普通下钻沿 FilesPane 原导航更新路径。
    pub(in crate::root) fn open_directory_in_host_workspace(
        &mut self,
        endpoint: FileEndpoint,
        lease: Option<ExternalTargetLease>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = (|| -> AppResult<TabId> {
            let (descriptor, transient) =
                super::super::super::workspace_file_navigation::file_navigation_descriptor(
                    &endpoint,
                    lease.as_ref(),
                )?;
            let WorkspaceSessionTab::WorkspaceFile {
                target, machine_id, ..
            } = descriptor
            else {
                return Err(AppError::invalid("目录目标无效"));
            };
            let owner = self.workspace_files.host_workspace_for_endpoint(&endpoint);
            let id = owner.clone().unwrap_or_default();
            let mut tab = if owner.is_some() {
                self.workspace_files
                    .tabs
                    .get(&id)
                    .ok_or_else(|| AppError::not_found("文件工作区已关闭"))?
                    .tab
                    .clone()
            } else {
                let name = match &endpoint {
                    FileEndpoint::Local { .. } => crate::locale::text("本机", "Local").to_string(),
                    FileEndpoint::Remote { host_id, .. } => self
                        .host_navigation
                        .as_ref()
                        .and_then(|view| view.read(cx).display_name_for(host_id))
                        .map(|name| name.to_string())
                        .unwrap_or_else(|| host_id.as_str().to_owned()),
                    FileEndpoint::Container { container_id, .. } => container_id.clone(),
                };
                WorkspaceSessionTab::FileWorkspace {
                    id: id.clone(),
                    tab_group_id: None,
                    title: format!("{} · {}", name, crate::locale::text("文件", "Files")),
                    machine_id,
                    target,
                    state: FileWorkspaceState::default(),
                }
            };
            let anchor = self
                .workspace_files
                .host_workspace_anchor(&id)
                .unwrap_or_else(|| endpoint.clone());
            let WorkspaceSessionTab::FileWorkspace { state, .. } = &mut tab else {
                return Err(AppError::conflict("文件宿主已变化"));
            };
            let item_id = state
                .items
                .iter()
                .find_map(|item| match item {
                    FileWorkspaceItem::Directory { id, path }
                        if endpoint_at_path(&anchor, path)
                            .is_ok_and(|current| current == endpoint) =>
                    {
                        Some(id.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(TabId::new);
            if !state.items.iter().any(|item| item.id() == &item_id) {
                state.items.push(FileWorkspaceItem::Directory {
                    id: item_id.clone(),
                    path: endpoint_path(&endpoint)?,
                });
            }
            state.active_item_id = Some(item_id);
            let controller = self
                .workspace_controller
                .as_mut()
                .ok_or_else(|| AppError::conflict("工作区尚未就绪"))?;
            let mut snapshot = controller.snapshot().clone();
            if let Some(current) = snapshot
                .terminal_tabs
                .iter_mut()
                .find(|current| current.id() == &id)
            {
                *current = tab.clone();
            } else {
                snapshot.terminal_tabs.push(tab.clone());
            }
            if let WorkspaceSessionTab::FileWorkspace { machine_id, .. } = &tab {
                snapshot.selected_machine_id = Some(machine_id.clone());
            }
            snapshot.active_tab_id = Some(id.clone());
            snapshot.focused_pane_id = None;
            controller.synchronize_snapshot(snapshot)?;
            self.workspace_files
                .install_host_descriptor(tab, Some(anchor), lease, transient);
            Ok(id)
        })();
        match result {
            Ok(id) => {
                self.activate_workspace_tab(&id, window, cx);
                self.persist_workspace_snapshot(cx);
            }
            Err(error) => {
                self.status = Some(error.public_message().into());
                cx.notify();
            }
        }
    }

    /// 以 owner 与内部身份区分共享缓冲：未装配描述只改导航，已装配文档仍须经过编辑器关闭授权。
    /// 目录和未装配文档都先通过完整快照事务，成功后才释放资源；失败不得取消仍在途的准备。
    /// @author kongweiguang
    pub(in crate::root) fn close_host_file_item(
        &mut self,
        owner: &TabId,
        item: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let gate = self
            .workspace_controller
            .as_ref()
            .ok_or_else(|| AppError::conflict("工作区尚未就绪"))
            .and_then(|controller| controller.ensure_no_pending_resource());
        if let Err(error) = gate {
            self.status = Some(error.public_message().into());
            cx.notify();
            return;
        }
        let Some((expected, target, was_active, owner_active)) =
            self.workspace_controller.as_ref().and_then(|controller| {
                let snapshot = controller.snapshot();
                let tab = snapshot
                    .terminal_tabs
                    .iter()
                    .find(|tab| tab.id() == owner)?;
                let WorkspaceSessionTab::FileWorkspace { state, target, .. } = tab else {
                    return None;
                };
                let entry = state.items.iter().find(|entry| entry.id() == item)?;
                Some((
                    entry.clone(),
                    target.clone(),
                    state.active_item_id.as_ref() == Some(item),
                    snapshot.active_tab_id.as_ref() == Some(owner),
                ))
            })
        else {
            self.status = Some(
                AppError::not_found("文件工作区标签已关闭")
                    .public_message()
                    .into(),
            );
            cx.notify();
            return;
        };
        let directory = matches!(&expected, FileWorkspaceItem::Directory { .. });
        let key = (owner.clone(), item.clone());
        if !directory {
            if let Some(runtime) = self.workspace_files.documents.get(&key) {
                let endpoint = runtime.endpoint.clone();
                let editor = self.file_editor_workspace.clone();
                let descriptor_matches =
                    self.workspace_files.tabs.get(owner).is_some_and(|runtime| {
                        matches!(&runtime.tab, WorkspaceSessionTab::FileWorkspace { state, target: runtime_target, .. }
                        if runtime_target == &target && state.items.iter().any(|entry| entry == &expected))
                    });
                let consistent = descriptor_matches
                    && endpoint.as_ref().is_some_and(|endpoint| {
                        self.workspace_files
                            .host_document_endpoint(owner, item)
                            .as_ref()
                            == Some(endpoint)
                    });
                let (Some(endpoint), Some(editor)) = (endpoint, editor) else {
                    self.status = Some(
                        AppError::conflict("文档运行态无法确认，请重试恢复")
                            .public_message()
                            .into(),
                    );
                    cx.notify();
                    return;
                };
                if !consistent {
                    self.status = Some(
                        AppError::conflict("文档运行态身份不一致，请重试恢复")
                            .public_message()
                            .into(),
                    );
                    cx.notify();
                    return;
                }
                if editor
                    .read(cx)
                    .workspace_projection()
                    .tabs
                    .iter()
                    .any(|tab| tab.endpoint == endpoint)
                {
                    editor.update(cx, |workspace, cx| workspace.request_close(endpoint, cx));
                    return;
                }
            }
        } else if let Some((pane, _)) = self.workspace_files.directories.get(&key)
            && pane.read(cx).file_workspace_directory_close_blocked()
        {
            self.status = Some(crate::locale::text(
                "目录仍有操作等待回执，请稍后重试关闭。",
                "This directory has pending operations. Retry closing after they finish.",
            ));
            cx.notify();
            return;
        }
        let result = self.update_host_file_state(owner, |state| {
            let entry = state
                .items
                .iter()
                .find(|entry| entry.id() == item)
                .ok_or_else(|| AppError::not_found("文件工作区标签已关闭"))?;
            if matches!(entry, FileWorkspaceItem::Directory { .. }) != directory {
                return Err(AppError::conflict("文件工作区标签类型已变化"));
            }
            if directory
                && state
                    .items
                    .iter()
                    .filter(|entry| matches!(entry, FileWorkspaceItem::Directory { .. }))
                    .count()
                    <= 1
            {
                return Err(AppError::conflict("保留最后一个目录页，便于继续浏览文件"));
            }
            state.items.retain(|entry| entry.id() != item);
            if state.active_item_id.as_ref() == Some(item) {
                state.active_item_id = state.items.first().map(|entry| entry.id().clone());
            }
            Ok(())
        });
        match result {
            Ok(()) => {
                if directory {
                    if let Some((pane, _)) = self.workspace_files.directories.remove(&key) {
                        pane.update(cx, |pane, cx| pane.cancel_transfer_preparation(cx));
                    }
                } else {
                    self.workspace_files.documents.remove(&key);
                }
                if was_active && owner_active && self.center_panel == CenterPanel::Editor {
                    self.activate_host_file_workspace(owner, window, cx);
                }
                self.persist_workspace_snapshot(cx);
                cx.notify();
            }
            Err(error) => {
                self.status = Some(error.public_message().into());
                cx.notify();
            }
        }
    }

    /// 迟到目录回执只能更新原 Entity 对应项，不能切换宿主或写入其它主机路径。
    pub(in crate::root) fn host_file_directory_changed(
        &mut self,
        source: &Entity<FilesPane>,
        endpoint: &FileEndpoint,
        cx: &mut Context<Self>,
    ) {
        let key = self
            .workspace_files
            .directories
            .iter()
            .find_map(|(key, (pane, _))| (pane == source).then(|| key.clone()));
        let Some((owner, item)) = key else { return };
        if source.read(cx).current_directory_endpoint() != endpoint {
            return;
        }
        let unchanged = endpoint_path(endpoint).is_ok_and(|path| self.workspace_files.tabs.get(&owner).is_some_and(|runtime| {
            matches!(&runtime.tab, WorkspaceSessionTab::FileWorkspace { state, .. } if state.items.iter().any(|entry| entry.id() == &item && entry.path() == path))
        }));
        if unchanged {
            return;
        }
        if !self
            .workspace_files
            .host_workspace_anchor(&owner)
            .is_some_and(|anchor| anchor.same_filesystem(endpoint))
        {
            return;
        }
        let result = endpoint_path(endpoint).and_then(|path| {
            self.update_host_file_state(&owner, |state| {
                let Some(FileWorkspaceItem::Directory { path: current, .. }) =
                    state.items.iter_mut().find(|entry| entry.id() == &item)
                else {
                    return Err(AppError::not_found("目录标签已关闭"));
                };
                *current = path;
                Ok(())
            })
        });
        if let Err(error) = result {
            // Entity 保留最新成功目录，门禁解除后从原 Entity 回投，不能重放旧请求。
            self.editor_navigation_sync_pending = true;
            self.status = Some(error.public_message().into());
        } else {
            self.persist_workspace_snapshot(cx);
        }
        cx.notify();
    }
}
