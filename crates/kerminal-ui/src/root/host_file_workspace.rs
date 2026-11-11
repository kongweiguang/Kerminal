//! 主机文件宿主只拥有导航与浏览实体，正文由共享编辑器唯一持有。
//!
//! @author kongweiguang

use super::*;
use kerminal_application::workspace::{FileWorkspaceItem, FileWorkspaceState, WorkspaceFileSource};

#[path = "host_file_workspace_directories.rs"]
mod directories;
#[path = "host_file_workspace_navigation.rs"]
mod navigation;
#[path = "host_file_workspace_projection.rs"]
mod projection;

#[cfg(test)]
#[path = "../../tests/unit/file_workspace_handoff_extra.rs"]
mod handoff_tests;
#[path = "host_file_workspace_render.rs"]
mod render;
#[path = "host_file_workspace_restore.rs"]
mod restore;
#[cfg(test)]
#[path = "../../tests/unit/root_host_file_workspace.rs"]
mod tests;

/// 替换路径而保留真实文件系统身份，临时容器不能被持久描述降级成本机。
fn endpoint_at_path(anchor: &FileEndpoint, path: &str) -> AppResult<FileEndpoint> {
    if path.is_empty() || path.chars().any(char::is_control) {
        return Err(AppError::invalid("文件工作区路径为空或包含控制字符"));
    }
    Ok(match anchor {
        FileEndpoint::Local { .. } => FileEndpoint::Local { path: path.into() },
        FileEndpoint::Remote { host_id, .. } => FileEndpoint::Remote {
            host_id: host_id.clone(),
            path: RemotePath::parse(path).map_err(|_| AppError::invalid("远端路径无效"))?,
        },
        FileEndpoint::Container {
            host_id,
            runtime,
            container_id,
            ..
        } => FileEndpoint::Container {
            host_id: host_id.clone(),
            runtime: *runtime,
            container_id: container_id.clone(),
            path: RemotePath::parse_container(path)
                .map_err(|_| AppError::invalid("容器路径无效"))?,
        },
    })
}

/// 本机路径必须无损持久化，不借远端 POSIX 规则重新解释。
fn endpoint_path(endpoint: &FileEndpoint) -> AppResult<String> {
    match endpoint {
        FileEndpoint::Local { path } => path
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| AppError::invalid("本机路径无法无损保存")),
        FileEndpoint::Remote { path, .. } | FileEndpoint::Container { path, .. } => {
            Ok(path.to_string())
        }
    }
}

/// 旧来源别名只参与查询，首项固定为新任务真实所属的宿主；关闭时复用冻结描述而非运行态。
pub(in crate::root) fn workspace_origins(
    id: &TabId,
    state: &FileWorkspaceState,
) -> Vec<TransferOrigin> {
    let mut origins = vec![TransferOrigin::WorkspaceFile(id.clone())];
    for alias in &state.source_tab_ids {
        let origin = TransferOrigin::WorkspaceFile(alias.clone());
        if !origins.contains(&origin) {
            origins.push(origin);
        }
    }
    origins
}

impl WorkspaceFileRuntime {
    /// 待恢复和失败宿主也参与文件系统去重，不能因尚未 Ready 产生第二个连接页。
    pub(in crate::root) fn host_workspace_for_endpoint(
        &self,
        endpoint: &FileEndpoint,
    ) -> Option<TabId> {
        self.tabs.iter().find_map(|(id, runtime)| {
            matches!(runtime.tab, WorkspaceSessionTab::FileWorkspace { .. })
                .then(|| self.host_workspace_anchor(id))
                .flatten()
                .filter(|anchor| anchor.same_filesystem(endpoint))
                .map(|_| id.clone())
        })
    }

    /// 临时目标优先真实端点，持久恢复只从明确目标推导，不借活动终端兜底。
    pub(in crate::root) fn host_workspace_anchor(&self, id: &TabId) -> Option<FileEndpoint> {
        let runtime = self.tabs.get(id)?;
        let WorkspaceSessionTab::FileWorkspace { target, state, .. } = &runtime.tab else {
            return None;
        };
        if let Some(endpoint) = &runtime.endpoint {
            return Some(endpoint.clone());
        }
        let item = state.items.first()?;
        let source = match target {
            WorkspaceTargetRef::Local { .. } => WorkspaceFileSource::Local,
            WorkspaceTargetRef::Ssh { .. } => WorkspaceFileSource::Sftp,
            WorkspaceTargetRef::DockerContainer { .. } => WorkspaceFileSource::Container,
            _ => return None,
        };
        workspace_file_endpoint_parts(target, item.path(), source).ok()
    }

    /// 文档键和宿主键使用不同索引，迁移沿用旧 ID 时不会覆盖宿主资源。
    pub(in crate::root) fn host_document_endpoint(
        &self,
        owner: &TabId,
        item: &TabId,
    ) -> Option<FileEndpoint> {
        let runtime = self.tabs.get(owner)?;
        let WorkspaceSessionTab::FileWorkspace { state, .. } = &runtime.tab else {
            return None;
        };
        let FileWorkspaceItem::Document { path, source, .. } =
            state.items.iter().find(|entry| entry.id() == item)?
        else {
            return None;
        };
        let anchor = self.host_workspace_anchor(owner)?;
        // 明确协议来源不能被工作区聚合抹掉，损坏描述不得借宿主端点换协议读取。
        if (*source == WorkspaceFileSource::Sftp && !matches!(anchor, FileEndpoint::Remote { .. }))
            || (*source == WorkspaceFileSource::Container
                && !matches!(anchor, FileEndpoint::Container { .. }))
        {
            return None;
        }
        endpoint_at_path(&anchor, path).ok()
    }

    /// 目录活动时不暴露隐藏文档为当前文件，避免保存和上下文动作作用于错误对象。
    pub(in crate::root) fn host_active_document(&self, owner: &TabId) -> Option<FileEndpoint> {
        let WorkspaceSessionTab::FileWorkspace { state, .. } = &self.tabs.get(owner)?.tab else {
            return None;
        };
        self.host_document_endpoint(owner, state.active_item_id.as_ref()?)
    }

    /// 目录活动时给宿主上下文行提供完整端点；锚点或路径无效时宁可不画，也不能把
    /// 另一个目录或文档目标冒充成当前选择。
    ///
    /// @author kongweiguang
    pub(in crate::root) fn host_active_directory_endpoint(
        &self,
        owner: &TabId,
    ) -> Option<FileEndpoint> {
        let WorkspaceSessionTab::FileWorkspace { state, .. } = &self.tabs.get(owner)?.tab else {
            return None;
        };
        let active = state.active_item_id.as_ref()?;
        let FileWorkspaceItem::Directory { path, .. } =
            state.items.iter().find(|item| item.id() == active)?
        else {
            return None;
        };
        endpoint_at_path(&self.host_workspace_anchor(owner)?, path).ok()
    }

    /// 关闭宿主核对全部文档，不能只给当前文档执行脏稿保护。
    pub(in crate::root) fn host_document_endpoints(&self, owner: &TabId) -> Vec<FileEndpoint> {
        let Some(WorkspaceFileTabRuntime {
            tab: WorkspaceSessionTab::FileWorkspace { state, .. },
            ..
        }) = self.tabs.get(owner)
        else {
            return Vec::new();
        };
        state
            .items
            .iter()
            .filter_map(|item| self.host_document_endpoint(owner, item.id()))
            .collect()
    }

    /// 浏览器从保留的目录初始化，不跟随每一次内部文件激活。
    pub(in crate::root) fn host_browser_target(
        &self,
        owner: &TabId,
    ) -> Option<(FileEndpoint, Option<ExternalTargetLease>)> {
        let runtime = self.tabs.get(owner)?;
        let WorkspaceSessionTab::FileWorkspace { state, .. } = &runtime.tab else {
            return None;
        };
        let path = state.items.iter().find_map(|item| match item {
            FileWorkspaceItem::Directory { path, .. } => Some(path),
            _ => None,
        })?;
        let endpoint = endpoint_at_path(&self.host_workspace_anchor(owner)?, path).ok()?;
        Some((endpoint, runtime.external_target_lease.clone()))
    }

    /// 每次成功打开都替换交接资格；来源不可快照时丢弃旧值，不能把它冒充本次操作。
    /// 快照只随当时的活动文档和宿主目录有效，避免延迟挂载时重放此前的导航。
    ///
    /// @author kongweiguang
    pub(in crate::root) fn replace_right_browser_context(
        &mut self,
        owner: &TabId,
        document: &FileEndpoint,
        snapshot: Option<crate::features::files::FileBrowserContextSnapshot>,
    ) {
        self.right_browser_contexts.remove(owner);
        let Some(snapshot) = snapshot else { return };
        if self.host_active_document(owner).as_ref() != Some(document) {
            return;
        }
        let Some((directory, _)) = self.file_browser_target(owner) else {
            return;
        };
        self.right_browser_contexts.insert(
            owner.clone(),
            RightBrowserContextHandoff {
                snapshot,
                document: document.clone(),
                directory,
            },
        );
    }

    /// 快照只在原文档和宿主目录仍适用时消费；其余情况一次性丢弃，不能覆盖后续用户操作。
    ///
    /// @author kongweiguang
    pub(in crate::root) fn take_right_browser_context(
        &mut self,
        owner: &TabId,
    ) -> Option<crate::features::files::FileBrowserContextSnapshot> {
        let handoff = self.right_browser_contexts.remove(owner)?;
        if self.host_active_document(owner).as_ref() != Some(&handoff.document)
            || self
                .file_browser_target(owner)
                .is_none_or(|(directory, _)| directory != handoff.directory)
        {
            tracing::debug!("文件宿主的文档或目录已变化，丢弃待交接的右栏浏览快照");
            return None;
        }
        Some(handoff.snapshot)
    }

    /// 顶层关闭必须收拢全部内部目录，不能只检查当前显示的一页。
    pub(in crate::root) fn host_directory_panes(&self, owner: &TabId) -> Vec<Entity<FilesPane>> {
        self.directories
            .iter()
            .filter(|((id, _), _)| id == owner)
            .map(|(_, (pane, _))| pane.clone())
            .collect()
    }

    /// 右栏与关闭门禁使用同一来源集合，不因当前文件切换遗漏旧恢复材料。
    pub(in crate::root) fn host_transfer_origins(&self, owner: &TabId) -> Vec<TransferOrigin> {
        match self.tabs.get(owner).map(|runtime| &runtime.tab) {
            Some(WorkspaceSessionTab::FileWorkspace { state, .. }) => {
                workspace_origins(owner, state)
            }
            _ => vec![TransferOrigin::WorkspaceFile(owner.clone())],
        }
    }

    /// controller 提交后再安装投影，既有正文和目录实体不随标签切换重建。
    fn install_host_descriptor(
        &mut self,
        tab: WorkspaceSessionTab,
        anchor: Option<FileEndpoint>,
        lease: Option<ExternalTargetLease>,
        transient: bool,
    ) {
        let id = tab.id().clone();
        let selection = |tab: &WorkspaceSessionTab| match tab {
            WorkspaceSessionTab::FileWorkspace { state, .. } => {
                state.active_item_id.as_ref().and_then(|active| {
                    // 宿主标签带只渲染文档，滚动索引必须使用文档序号，目录项不应把索引顶偏。
                    state
                        .items
                        .iter()
                        .filter(|item| matches!(item, FileWorkspaceItem::Document { .. }))
                        .position(|item| item.id() == active)
                        .map(|index| (active.clone(), index))
                })
            }
            _ => None,
        };
        let previous = self
            .tabs
            .get(&id)
            .and_then(|runtime| selection(&runtime.tab));
        let next = selection(&tab);
        let scroll = self.host_tab_scroll_handles.entry(id.clone()).or_default();
        // 只随显式活动项/顺序改变申请显露，普通投影不抢用户的手动滚动。
        if previous != next
            && let Some((_, index)) = next
        {
            scroll.scroll_to_item(index);
        }
        if let Some(runtime) = self.tabs.get_mut(&id) {
            runtime.tab = tab;
            if anchor.is_some() {
                runtime.endpoint = anchor;
            }
            if lease.is_some() {
                runtime.external_target_lease = lease;
            }
            runtime.transient |= transient;
        } else {
            self.tabs.insert(
                id,
                WorkspaceFileTabRuntime {
                    tab,
                    status: WorkspaceFileRestoreState::Pending,
                    endpoint: anchor,
                    files_pane: None,
                    files_subscriptions: None,
                    external_target_lease: lease,
                    transient,
                    pending_transfer_endpoints: None,
                },
            );
        }
    }
}
