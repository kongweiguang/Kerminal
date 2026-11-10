//! 主机工作区的显式导航事务，不由编辑器投影反向创建顶层标签。
//!
//! @author kongweiguang

use super::*;
use crate::features::workspace_tabs::WorkspaceTabEvent;

impl RootView {
    /// 先冻结原文件描述，再将其挂入唯一宿主；失败不改变原浏览上下文。
    pub(in crate::root) fn open_file_in_host_workspace(
        &mut self,
        endpoint: FileEndpoint,
        lease: Option<ExternalTargetLease>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let result = super::super::super::workspace_file_navigation::file_navigation_descriptor(
            &endpoint,
            lease.as_ref(),
        )
        .and_then(|(document, transient)| {
            self.open_host_document_descriptor(document, endpoint, lease, transient, cx)
        });
        match result {
            Ok(id) => {
                self.refresh_workspace_views(cx);
                self.activate_workspace_tab(&id, window, cx);
                self.persist_workspace_snapshot(cx);
                true
            }
            Err(error) => {
                self.status = Some(error.public_message().into());
                cx.notify();
                false
            }
        }
    }

    /// 已有文档只收紧访问策略；先检查编辑服务以保持错误优先级，普通打开绝不放宽只读保护。
    /// @author kongweiguang
    pub(in crate::root) fn open_host_document_descriptor(
        &mut self,
        document: WorkspaceSessionTab,
        endpoint: FileEndpoint,
        lease: Option<ExternalTargetLease>,
        transient: bool,
        cx: &mut Context<Self>,
    ) -> AppResult<TabId> {
        let editor = self
            .file_editor_workspace
            .clone()
            .ok_or_else(|| AppError::conflict("文件编辑服务尚未配置"))?;
        let WorkspaceSessionTab::WorkspaceFile {
            id: document_id,
            title: document_title,
            machine_id,
            target,
            path,
            root_path,
            access,
            source,
            ..
        } = document
        else {
            return Err(AppError::invalid("文件宿主只接受明确文档描述"));
        };
        if access == WorkspaceFileAccess::Readonly {
            // 即使缓冲来自独立编辑页，也先拒绝脏稿/在途保存，不能先提交一个收紧失败的宿主。
            editor.update(cx, |workspace, cx| {
                workspace.workspace_file_readonly_before_restriction(&endpoint, cx)
            })?;
        }
        let owner = self.workspace_files.host_workspace_for_endpoint(&endpoint);
        let mut tab = if let Some(owner) = &owner {
            self.workspace_files
                .tabs
                .get(owner)
                .map(|runtime| runtime.tab.clone())
                .ok_or_else(|| AppError::conflict("文件宿主已关闭"))?
        } else {
            let directory = crate::features::files::parent_endpoint(&endpoint)
                .ok_or_else(|| AppError::invalid("无法确定文件父目录"))?;
            let directory_id = TabId::new();
            let name = match &endpoint {
                FileEndpoint::Local { .. } => crate::locale::text("本机", "Local").to_string(),
                FileEndpoint::Remote { host_id, .. } => self
                    .host_navigation
                    .as_ref()
                    .and_then(|navigation| navigation.read(cx).display_name_for(host_id))
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| host_id.as_str().to_owned()),
                FileEndpoint::Container { container_id, .. } => container_id.clone(),
            };
            WorkspaceSessionTab::FileWorkspace {
                id: TabId::new(),
                tab_group_id: None,
                title: format!("{} · {}", name, crate::locale::text("文件", "Files")),
                machine_id,
                target,
                state: FileWorkspaceState {
                    items: vec![FileWorkspaceItem::Directory {
                        id: directory_id.clone(),
                        path: endpoint_path(&directory)?,
                    }],
                    active_item_id: Some(directory_id),
                    source_tab_ids: Vec::new(),
                },
            }
        };
        let owner_id = tab.id().clone();
        let anchor = self
            .workspace_files
            .host_workspace_anchor(&owner_id)
            .unwrap_or_else(|| endpoint.clone());
        let WorkspaceSessionTab::FileWorkspace { state, .. } = &mut tab else {
            return Err(AppError::conflict("文件宿主类型已变化"));
        };
        let existing = state.items.iter().position(|item| {
            matches!(item, FileWorkspaceItem::Document { path, .. }
            if endpoint_at_path(&anchor, path).is_ok_and(|value| value == endpoint))
        });
        if let Some(index) = existing {
            if access == WorkspaceFileAccess::Readonly
                && let FileWorkspaceItem::Document { access, .. } = &mut state.items[index]
            {
                *access = WorkspaceFileAccess::Readonly;
            }
            state.active_item_id = Some(state.items[index].id().clone());
        } else {
            state.items.push(FileWorkspaceItem::Document {
                id: document_id.clone(),
                title: document_title,
                path,
                root_path,
                access,
                source,
            });
            state.active_item_id = Some(document_id);
        }
        let mut snapshot = self
            .workspace_controller
            .as_ref()
            .ok_or_else(|| AppError::conflict("工作区尚未就绪"))?
            .snapshot()
            .clone();
        if let Some(current) = snapshot
            .terminal_tabs
            .iter_mut()
            .find(|entry| entry.id() == &owner_id)
        {
            *current = tab.clone();
        } else {
            snapshot.terminal_tabs.push(tab.clone());
        }
        if let WorkspaceSessionTab::FileWorkspace { machine_id, .. } = &tab {
            snapshot.selected_machine_id = Some(machine_id.clone());
        }
        snapshot.active_tab_id = Some(owner_id.clone());
        snapshot.focused_pane_id = None;
        self.workspace_controller
            .as_mut()
            .ok_or_else(|| AppError::conflict("工作区已失效"))?
            .synchronize_snapshot(snapshot)?;
        self.workspace_files
            .install_host_descriptor(tab, Some(anchor), lease, transient);
        // 新文档的缓冲在激活入口装配，状态索引先提交以保证连点复用。
        if let Some(runtime) = self.workspace_files.tabs.get_mut(&owner_id) {
            runtime.status = WorkspaceFileRestoreState::Pending;
        }
        Ok(owner_id)
    }

    /// 已打开文件的快捷入口先定位内部项，不能只激活宿主后又恢复另一份活动文档。
    pub(in crate::root) fn activate_registered_host_document(
        &mut self,
        endpoint: &FileEndpoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(owner) = self.workspace_files.host_workspace_for_endpoint(endpoint) else {
            return false;
        };
        let item = self.workspace_files.tabs.get(&owner).and_then(|runtime| {
            let WorkspaceSessionTab::FileWorkspace { state, .. } = &runtime.tab else {
                return None;
            };
            state.items.iter().find_map(|item| {
                (self
                    .workspace_files
                    .host_document_endpoint(&owner, item.id())
                    .as_ref()
                    == Some(endpoint))
                .then(|| item.id().clone())
            })
        });
        let Some(item) = item else { return false };
        self.activate_host_file_item(&owner, &item, window, cx);
        true
    }

    /// 内部切换只变更宿主活动项，不生成顶层导航事件或重置其它目录实体。
    pub(in crate::root) fn activate_host_file_item(
        &mut self,
        owner: &TabId,
        item_id: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = self.update_host_file_state(owner, |state| {
            if !state.items.iter().any(|item| item.id() == item_id) {
                return Err(AppError::not_found("文件工作区标签已关闭"));
            }
            state.active_item_id = Some(item_id.clone());
            Ok(())
        });
        if let Err(error) = result {
            self.status = Some(error.public_message().into());
            cx.notify();
            return;
        }
        if self.handle_workspace_tab_event(&WorkspaceTabEvent::Activate(owner.clone()), window, cx)
        {
            self.activate_workspace_tab(owner, window, cx);
        }
        self.persist_workspace_snapshot(cx);
    }

    /// 目录路径和内部顺序经同一 controller 门禁提交，失败时运行态仍与原快照一致。
    pub(super) fn update_host_file_state(
        &mut self,
        owner: &TabId,
        update: impl FnOnce(&mut FileWorkspaceState) -> AppResult<()>,
    ) -> AppResult<()> {
        let controller = self
            .workspace_controller
            .as_mut()
            .ok_or_else(|| AppError::conflict("工作区尚未就绪"))?;
        let mut snapshot = controller.snapshot().clone();
        let tab = snapshot
            .terminal_tabs
            .iter_mut()
            .find(|tab| tab.id() == owner)
            .ok_or_else(|| AppError::not_found("文件工作区已关闭"))?;
        let WorkspaceSessionTab::FileWorkspace { state, .. } = tab else {
            return Err(AppError::conflict("工作区类型已变化"));
        };
        update(state)?;
        let updated = tab.clone();
        controller.synchronize_snapshot(snapshot)?;
        self.workspace_files
            .install_host_descriptor(updated, None, None, false);
        Ok(())
    }
}
