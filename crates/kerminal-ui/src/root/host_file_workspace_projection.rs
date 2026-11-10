//! 编辑器发布的是文档事实，不再与顶层工作区一对一互相驱动。
//!
//! @author kongweiguang

use std::collections::{HashMap, HashSet};

use super::*;

impl RootView {
    /// 只删除已装配且编辑器确认关闭的文档；先准备文件宿主，提交成功后才回收资源。
    /// @author kongweiguang
    pub(in crate::root) fn sync_host_editor_projection(
        &mut self,
        projection: &EditorWorkspaceProjection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(controller) = self.workspace_controller.as_ref() else {
            return;
        };
        let snapshot = controller.snapshot();
        let directory_visible = self.center_panel == CenterPanel::Editor && snapshot.terminal_tabs.iter().any(|tab| {
            matches!(tab, WorkspaceSessionTab::FileWorkspace { id, state, .. }
                if snapshot.active_tab_id.as_ref() == Some(id) && !matches!(state.active_item(), Some(FileWorkspaceItem::Document { .. })))
        });
        if directory_visible
            && projection.active_endpoint.is_some()
            && let Some(editor) = self.file_editor_workspace.clone()
        {
            editor.update(cx, |editor, cx| editor.deactivate_document(cx));
        }
        let projected_endpoints = projection
            .tabs
            .iter()
            .map(|tab| &tab.endpoint)
            .collect::<HashSet<_>>();
        let mut updated = Vec::new();
        let mut changed = false;
        for (index, original) in snapshot.terminal_tabs.iter().enumerate() {
            if !matches!(original, WorkspaceSessionTab::FileWorkspace { .. }) {
                continue;
            }
            let mut tab = original.clone();
            let WorkspaceSessionTab::FileWorkspace { id, state, .. } = &mut tab else {
                continue;
            };
            // 持久化或关闭门禁期间的导航仍保留在原目录实体；解除后回读最新已提交位置。
            for item in &mut state.items {
                if let FileWorkspaceItem::Directory { id: item_id, path } = item
                    && let Some((pane, _)) = self
                        .workspace_files
                        .directories
                        .get(&(id.clone(), item_id.clone()))
                {
                    match endpoint_path(pane.read(cx).current_directory_endpoint()) {
                        Ok(current) => *path = current,
                        Err(error) => self.status = Some(error.public_message().into()),
                    }
                }
            }
            let was_document = state.active_item_id.as_ref().is_some_and(|active| {
                state.items.iter().any(|item| {
                    item.id() == active && matches!(item, FileWorkspaceItem::Document { .. })
                })
            });
            state.items.retain(|item| {
                let FileWorkspaceItem::Document { id: item_id, .. } = item else {
                    return true;
                };
                let Some(runtime) = self
                    .workspace_files
                    .documents
                    .get(&(id.clone(), item_id.clone()))
                else {
                    return true;
                };
                runtime.status != WorkspaceFileRestoreState::Ready
                    || runtime
                        .endpoint
                        .as_ref()
                        .is_some_and(|endpoint| projected_endpoints.contains(endpoint))
            });
            if was_document {
                let projected_active = projection.active_endpoint.as_ref().and_then(|endpoint| {
                    let runtime = self.workspace_files.tabs.get(id)?;
                    let WorkspaceSessionTab::FileWorkspace {
                        state: runtime_state,
                        ..
                    } = &runtime.tab
                    else {
                        return None;
                    };
                    let anchor = self.workspace_files.host_workspace_anchor(id)?;
                    // 先按路径缩小候选，最终仍交给原端点解析校验协议和宿主身份。
                    let candidates = runtime_state
                        .items
                        .iter()
                        .filter_map(|item| match item {
                            FileWorkspaceItem::Document { id, path, .. }
                                if endpoint_at_path(&anchor, path)
                                    .is_ok_and(|value| &value == endpoint) =>
                            {
                                Some(id)
                            }
                            _ => None,
                        })
                        .collect::<HashSet<_>>();
                    state.items.iter().find_map(|item| {
                        (candidates.contains(item.id())
                            && self
                                .workspace_files
                                .host_document_endpoint(id, item.id())
                                .as_ref()
                                == Some(endpoint))
                        .then(|| item.id().clone())
                    })
                });
                if projected_active.is_some() {
                    state.active_item_id = projected_active;
                }
            }
            if !state
                .active_item_id
                .as_ref()
                .is_some_and(|active| state.items.iter().any(|item| item.id() == active))
            {
                state.active_item_id = state
                    .items
                    .iter()
                    .find(|item| matches!(item, FileWorkspaceItem::Directory { .. }))
                    .or_else(|| state.items.first())
                    .map(|item| item.id().clone());
            }
            changed |= &tab != original;
            updated.push((index, tab));
        }
        if !changed {
            return;
        }
        // 只有确有变化才复制完整快照；提交前不复制无关终端和扩展标签。
        let mut snapshot = controller.snapshot().clone();
        for (index, tab) in &updated {
            snapshot.terminal_tabs[*index] = tab.clone();
        }
        if let Err(error) = self
            .workspace_controller
            .as_mut()
            .ok_or_else(|| AppError::conflict("工作区已失效"))
            .and_then(|controller| controller.synchronize_snapshot(snapshot))
        {
            self.status = Some(error.public_message().into());
            cx.notify();
            return;
        }
        // 一次核对所有宿主成员，未知 owner 保持原样，且不能在提交失败时释放资源。
        let members = updated
            .iter()
            .filter_map(|(_, tab)| match tab {
                WorkspaceSessionTab::FileWorkspace { id, state, .. } => Some((
                    id,
                    state
                        .items
                        .iter()
                        .map(FileWorkspaceItem::id)
                        .collect::<HashSet<_>>(),
                )),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        self.workspace_files
            .documents
            .retain(|(owner, item), _| members.get(owner).is_none_or(|items| items.contains(item)));
        for (_, tab) in updated {
            self.workspace_files
                .install_host_descriptor(tab, None, None, false);
        }
        let active = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.snapshot().active_tab_id.clone());
        if self.center_panel == CenterPanel::Editor
            && let Some(active) = active
        {
            self.activate_host_file_workspace(&active, window, cx);
        }
        self.persist_workspace_snapshot(cx);
    }
}
