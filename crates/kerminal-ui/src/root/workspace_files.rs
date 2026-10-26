//! 按稳定 TabId 恢复非终端工作区资源，避免不同文件面板共享可变目录状态。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, path::Path};

use gpui_kit::{AppContext as _, Context, Entity, SharedString, Subscription, Window};
use kerminal_application::{
    AppError, AppResult,
    workspace::{
        WorkspaceSessionSnapshot, WorkspaceSessionTab, WorkspaceTargetRef,
        WorkspaceTargetUnavailableReason,
    },
};
use kerminal_domain::{
    files::{ContainerRuntime, FileEndpoint, RemotePath},
    identity::{HostId, TabId},
};

use crate::features::{
    editor::EditorWorkspaceProjection,
    files::{
        ActivateEditorFileRequested, CreateSftpHostRequested, FilesPane, OpenFileRequested,
        SftpHostRequested, WorkspaceOverviewRequested,
    },
    workspace_controller::{WorkspaceRestoreFailure, WorkspaceRestoreItemId},
    workspace_tabs::WorkspaceFileTabAction,
};

use super::{CenterPanel, RootView};

/// 中央共享工作台与独立传输标签显式区分，避免后台活动标签覆盖用户刚打开的工作台。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum FilesWorkspaceSelection {
    #[default]
    Workbench,
    TransferTab(TabId),
}

impl FilesWorkspaceSelection {
    /// 关闭 ACK 后只修正已移除的展示来源；后继来自 reducer 快照，后台标签关闭不抢共享工作台。
    ///
    /// @author kongweiguang
    fn reconcile_removed_tab(
        &mut self,
        removed_id: &TabId,
        snapshot: Option<&WorkspaceSessionSnapshot>,
    ) {
        if !matches!(self, Self::TransferTab(tab_id) if tab_id == removed_id) {
            return;
        }
        *self = snapshot
            .and_then(|snapshot| {
                let active_id = snapshot.active_tab_id.as_ref()?;
                snapshot.terminal_tabs.iter().find_map(|tab| match tab {
                    WorkspaceSessionTab::SftpTransfer { id, .. }
                        if id == active_id && id != removed_id =>
                    {
                        Some(Self::TransferTab(id.clone()))
                    }
                    _ => None,
                })
            })
            .unwrap_or_default();
    }
}

/// 每个非终端 tab 保留自己的 descriptor、加载状态与 SFTP 面板实体。
#[derive(Default)]
pub(super) struct WorkspaceFileRuntime {
    tabs: BTreeMap<TabId, WorkspaceFileTabRuntime>,
    snapshot_installed: bool,
}

/// 运行态只持有该 tab 的资源，不复制文件服务或改变 snapshot 中的业务身份。
struct WorkspaceFileTabRuntime {
    tab: WorkspaceSessionTab,
    status: WorkspaceFileRestoreState,
    endpoint: Option<FileEndpoint>,
    files_pane: Option<Entity<FilesPane>>,
    files_subscriptions: Option<FilesPaneSubscriptions>,
}

/// 一个 FilesPane 的 Root 桥接和删除观察器同生共灭，避免工作区标签漏接安全边界。
pub(super) struct FilesPaneSubscriptions {
    _open_file: Subscription,
    _workspace_overview: Subscription,
    _activate_editor_file: Subscription,
    _sftp_host: Subscription,
    _create_sftp_host: Subscription,
    _delete_confirmation_observer: Subscription,
}

/// 显式区分尚未打开、可用和失败；失败的原始 typed tab 仍留在 runtime/snapshot。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum WorkspaceFileRestoreState {
    Pending,
    Ready,
    Failed {
        failure: WorkspaceRestoreFailure,
        message: SharedString,
    },
}

/// 双面板 SFTP 恢复所需的已验证 endpoint；缺少初始本地目录的旧 tab 延续工作区默认目录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SftpTransferEndpoints {
    pub left: FileEndpoint,
    pub right: FileEndpoint,
    pub locked_left_host_id: Option<HostId>,
    pub initial_right_selection: Option<String>,
}

/// 恢复失败类型保持稳定，UI 提示不泄露 host 配置或本地文件系统错误正文。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspaceFileRestoreIssue {
    MissingTarget,
    UnsupportedTarget,
    InvalidPath,
    ExternalTarget,
    Unavailable(WorkspaceTargetUnavailableReason),
}

impl WorkspaceFileRuntime {
    /// 按原 snapshot 建立非终端 tab 的待恢复索引，不在无 Window 的数据回调里造 UI Entity。
    pub fn from_snapshot(snapshot: &WorkspaceSessionSnapshot) -> Self {
        let mut tabs = BTreeMap::new();
        for tab in &snapshot.terminal_tabs {
            if matches!(tab, WorkspaceSessionTab::Terminal { .. }) {
                continue;
            }
            tabs.insert(
                tab.id().clone(),
                WorkspaceFileTabRuntime {
                    tab: tab.clone(),
                    status: WorkspaceFileRestoreState::Pending,
                    endpoint: None,
                    files_pane: None,
                    files_subscriptions: None,
                },
            );
        }
        Self {
            tabs,
            snapshot_installed: true,
        }
    }

    /// Register only a persistent SFTP descriptor after the initial load, leaving actual FilesPane creation to restore.
    ///
    /// @author kongweiguang
    pub(super) fn register_pending_sftp_transfer_tab(&mut self, tab: WorkspaceSessionTab) -> bool {
        if !self.snapshot_installed
            || !matches!(
                &tab,
                WorkspaceSessionTab::SftpTransfer {
                    external_launch_id: None,
                    ..
                }
            )
            || self.tabs.contains_key(tab.id())
        {
            return false;
        }
        let tab_id = tab.id().clone();
        self.tabs.insert(
            tab_id,
            WorkspaceFileTabRuntime {
                tab,
                status: WorkspaceFileRestoreState::Pending,
                endpoint: None,
                files_pane: None,
                files_subscriptions: None,
            },
        );
        true
    }

    /// 返回尚未执行的 ID，顺序固定以便启动恢复可复现且不依赖 HashMap 随机迭代。
    ///
    /// @author kongweiguang
    pub fn pending_tab_ids(&self) -> Vec<TabId> {
        self.tabs
            .iter()
            .filter(|(_, tab)| tab.status == WorkspaceFileRestoreState::Pending)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// 供 Root 渲染活动 SFTP tab；每个 tab 的目录和任务视图拥有独立 FilesPane Entity。
    pub fn files_pane(&self, tab_id: &TabId) -> Option<Entity<FilesPane>> {
        self.tabs.get(tab_id)?.files_pane.clone()
    }

    /// 收集当前已恢复的文件面板实体，供语言切换更新缓存控件而不重建每个 workspace tab。
    ///
    /// @author kongweiguang
    pub(super) fn files_panes(&self) -> Vec<Entity<FilesPane>> {
        self.tabs
            .values()
            .filter_map(|tab| tab.files_pane.clone())
            .collect()
    }

    /// 供 Root 激活对应的共享原生编辑器文档，不把 FileEndpoint 写成新的领域身份。
    pub fn file_endpoint(&self, tab_id: &TabId) -> Option<FileEndpoint> {
        self.tabs.get(tab_id)?.endpoint.clone()
    }

    /// 关闭一个 workspace 别名前确认是否仍有其它标签引用同一编辑文档。
    pub fn has_other_workspace_file_endpoint(
        &self,
        closing_tab_id: &TabId,
        endpoint: &FileEndpoint,
    ) -> bool {
        self.tabs.iter().any(|(tab_id, tab)| {
            tab_id != closing_tab_id && tab.endpoint.as_ref() == Some(endpoint)
        })
    }

    /// 多个 tab 共享同一 endpoint 时只读策略取最严格值，防止活动 alias 放宽已声明限制。
    pub fn workspace_file_requires_readonly(&self, endpoint: &FileEndpoint) -> bool {
        self.tabs.values().any(|tab| {
            let WorkspaceSessionTab::WorkspaceFile {
                target,
                path,
                access,
                source,
                ..
            } = &tab.tab
            else {
                return false;
            };
            *access == kerminal_application::workspace::WorkspaceFileAccess::Readonly
                && workspace_file_endpoint_parts(target, path, *source)
                    .is_ok_and(|resolved| &resolved == endpoint)
        })
    }

    /// Root 可把待恢复项和可见失败映射到对应 workspace tab 的状态提示。
    pub fn restore_state(&self, tab_id: &TabId) -> Option<&WorkspaceFileRestoreState> {
        self.tabs.get(tab_id).map(|tab| &tab.status)
    }

    /// 首次读取后只安装一次快照；没有已完成的关闭门禁时拒绝替换活动 Entity。
    pub fn install_snapshot(&mut self, snapshot: &WorkspaceSessionSnapshot) -> AppResult<()> {
        if self.snapshot_installed {
            return Err(AppError::conflict(
                "非终端工作区仍持有活动资源，必须先完成关闭流程再替换快照",
            ));
        }
        *self = Self::from_snapshot(snapshot);
        Ok(())
    }

    /// 只有显式用户重试才能重新执行失败的文件目标解析或面板装配。
    pub fn retry_failed_tab(&mut self, tab_id: &TabId) -> bool {
        let Some(tab) = self.tabs.get_mut(tab_id) else {
            return false;
        };
        if !matches!(tab.status, WorkspaceFileRestoreState::Failed { .. }) {
            return false;
        }
        tab.status = WorkspaceFileRestoreState::Pending;
        tab.endpoint = None;
        true
    }

    /// 更新恢复结果并保留 tab DTO，供失败原因显示和显式重试使用。
    fn set_result(
        &mut self,
        tab_id: &TabId,
        result: Result<Option<FileEndpoint>, (WorkspaceRestoreFailure, SharedString)>,
    ) {
        let Some(tab) = self.tabs.get_mut(tab_id) else {
            return;
        };
        match result {
            Ok(endpoint) => {
                tab.endpoint = endpoint;
                tab.status = WorkspaceFileRestoreState::Ready;
            }
            Err((failure, message)) => {
                tab.status = WorkspaceFileRestoreState::Failed { failure, message };
            }
        }
    }

    /// 安装独立 FilesPane 并保活其 Root 事件组，避免恢复的 SFTP 面板漏接文件与编辑器同步。
    ///
    /// @author kongweiguang
    fn set_files_pane(
        &mut self,
        tab_id: &TabId,
        pane: Entity<FilesPane>,
        subscriptions: FilesPaneSubscriptions,
    ) {
        let Some(tab) = self.tabs.get_mut(tab_id) else {
            return;
        };
        tab.files_pane = Some(pane);
        tab.files_subscriptions = Some(subscriptions);
        tab.status = WorkspaceFileRestoreState::Ready;
    }
}

impl RootView {
    /// 为每个文件视图绑定同一组 typed Root 桥接；缓存中的右栏面板不能回投导航或开启根确认。
    /// 主机选择仅受理共享工作台的显式意图，复用保存连接的信任预检，不让文件视图自行建连接。
    /// 新建主机同样交给 Root 捕获来源身份，保存成功不依赖当时的后台活动传输标签。
    ///
    /// @author kongweiguang
    pub(super) fn subscribe_files_pane_events(
        pane: &Entity<FilesPane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> FilesPaneSubscriptions {
        let open_file = cx.subscribe_in(
            pane,
            window,
            |root, source, event: &OpenFileRequested, window, cx| {
                if root.is_cached_sftp_panel(source) {
                    return;
                }
                root.open_file_endpoint(event.endpoint.clone(), window, cx);
            },
        );
        let workspace_overview = cx.subscribe_in(
            pane,
            window,
            |root, source, event: &WorkspaceOverviewRequested, _, cx| {
                if root.is_cached_sftp_panel(source) {
                    return;
                }
                root.return_editor_workspace_projection(source.clone(), event.generation, cx);
            },
        );
        let activate_editor_file = cx.subscribe_in(
            pane,
            window,
            |root, source, event: &ActivateEditorFileRequested, window, cx| {
                if root.is_cached_sftp_panel(source) {
                    return;
                }
                root.activate_open_editor_file(event.endpoint.clone(), window, cx);
            },
        );
        let sftp_host = cx.subscribe_in(
            pane,
            window,
            |root, source, event: &SftpHostRequested, window, cx| {
                if root.files_pane.as_ref() != Some(source) {
                    return;
                }
                super::connections::handle_host_navigation_event(
                    root,
                    &crate::features::connections::HostNavigationEvent::OpenSftpRequested {
                        host_id: event.host_id.clone(),
                    },
                    window,
                    cx,
                );
            },
        );
        let create_sftp_host = cx.subscribe_in(
            pane,
            window,
            |root, source, _: &CreateSftpHostRequested, window, cx| {
                if root.files_pane.as_ref() != Some(source) {
                    return;
                }
                root.begin_create_sftp_host_from_workbench(window, cx);
            },
        );
        let delete_confirmation_observer =
            cx.observe_in(pane, window, |root, source, window, cx| {
                let active = source.read(cx).pending_delete_confirmation().is_some();
                if active && root.is_cached_sftp_panel(&source) {
                    return;
                }
                root.sync_file_delete_root_confirmation(source.clone(), active, window, cx);
            });
        FilesPaneSubscriptions {
            _open_file: open_file,
            _workspace_overview: workspace_overview,
            _activate_editor_file: activate_editor_file,
            _sftp_host: sftp_host,
            _create_sftp_host: create_sftp_host,
            _delete_confirmation_observer: delete_confirmation_observer,
        }
    }

    /// 只读复制当前 Editor 投影并原样回带 request generation，Files 会丢弃过期响应。
    ///
    /// @author kongweiguang
    pub(super) fn return_editor_workspace_projection(
        &mut self,
        source: Entity<FilesPane>,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self
            .file_editor_workspace
            .as_ref()
            .map(|editor| editor.read(cx).workspace_projection())
            .unwrap_or(EditorWorkspaceProjection {
                revision: 0,
                tabs: Vec::new(),
                active_endpoint: None,
            });
        source.update(cx, |files, cx| {
            files.set_workspace_projection(snapshot, generation, cx);
        });
    }

    /// 将编辑器状态推给独立面板；最后一个临时编辑页关闭后只返回原本选中的终端，不创建或改绑会话。
    ///
    /// @author kongweiguang
    pub(super) fn push_editor_workspace_projection(
        &mut self,
        projection: EditorWorkspaceProjection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active_tab_is_terminal = self
            .workspace_controller
            .as_ref()
            .is_some_and(|controller| {
                let snapshot = controller.snapshot();
                snapshot.active_tab_id.as_ref().is_some_and(|id| {
                    snapshot
                        .terminal_tabs
                        .iter()
                        .any(|tab| tab.id() == id && tab.terminal_layout().is_some())
                })
            });
        if !self.host_shutdown_requested
            && should_return_to_terminal_after_editor_close(
                self.center_panel,
                &projection,
                active_tab_is_terminal,
            )
        {
            self.set_center_panel(CenterPanel::Terminal, window, cx);
        }
        for files in self.files_pane.iter().chain(self.sftp_panel.iter()) {
            let snapshot = projection.clone();
            files.update(cx, |files, cx| {
                files.set_live_workspace_projection(snapshot, cx);
            });
        }
        for files in self.workspace_files.files_panes() {
            let snapshot = projection.clone();
            files.update(cx, |files, cx| {
                files.set_live_workspace_projection(snapshot, cx);
            });
        }
    }

    /// 只切换仍存在的编辑器 endpoint；关闭后的 Files 列表项不会重开文件或更换中心页。
    ///
    /// @author kongweiguang
    pub(super) fn activate_open_editor_file(
        &mut self,
        endpoint: FileEndpoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.file_editor_workspace.clone() else {
            return;
        };
        let activated = editor.update(cx, |workspace, cx| {
            let projection = workspace.workspace_projection();
            if !editor_projection_contains_endpoint(&projection, &endpoint) {
                return false;
            }
            workspace.activate_open_file(endpoint, cx)
        });
        if activated {
            self.set_center_panel(CenterPanel::Editor, window, cx);
        }
    }

    /// 在无副作用的 workspace load 回调中仅建立 ID 索引，Window Entity 留给真实 render 生命周期。
    pub(super) fn install_workspace_file_snapshot(
        &mut self,
        snapshot: &WorkspaceSessionSnapshot,
    ) -> AppResult<()> {
        self.workspace_files.install_snapshot(snapshot)
    }

    /// 恢复全部非终端 tab 时只在真实 Window 上创建 UI；已完成或失败项不被重复启动。
    pub(super) fn restore_workspace_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending = self.workspace_files.pending_tab_ids();
        for tab_id in pending {
            self.ensure_workspace_file_tab(&tab_id, window, cx);
        }
    }

    /// 激活或延迟创建单个非终端资源；所有失败都保留原 descriptor 并更新可见状态。
    pub(super) fn ensure_workspace_file_tab(
        &mut self,
        tab_id: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self
            .workspace_files
            .tabs
            .get(tab_id)
            .map(|runtime| runtime.tab.clone())
        else {
            return;
        };
        if self
            .workspace_files
            .restore_state(tab_id)
            .is_some_and(|state| *state != WorkspaceFileRestoreState::Pending)
        {
            self.activate_workspace_file_tab(tab_id, cx);
            return;
        }

        match tab {
            WorkspaceSessionTab::WorkspaceFile {
                title,
                target,
                path,
                access,
                source,
                ..
            } => {
                let endpoint = match workspace_file_endpoint_parts(&target, &path, source) {
                    Ok(endpoint) => endpoint,
                    Err(issue) => {
                        self.finish_workspace_file_restore(
                            tab_id,
                            Err(issue.restore_failure()),
                            issue.message(),
                            cx,
                        );
                        return;
                    }
                };
                let readonly = self
                    .workspace_files
                    .workspace_file_requires_readonly(&endpoint)
                    || access == kerminal_application::workspace::WorkspaceFileAccess::Readonly;
                let Some(editor) = self.file_editor_workspace.clone() else {
                    self.finish_workspace_file_restore(
                        tab_id,
                        Err(WorkspaceRestoreFailure::ResourceFailure),
                        crate::locale::text(
                            "文件编辑服务尚未配置。",
                            "The file editor service is unavailable.",
                        ),
                        cx,
                    );
                    return;
                };
                let result = editor.update(cx, |workspace, cx| {
                    workspace.open_workspace_file(endpoint.clone(), title, readonly, cx)
                });
                match result {
                    Ok(()) => self.finish_workspace_file_restore(
                        tab_id,
                        Ok(Some(endpoint)),
                        SharedString::default(),
                        cx,
                    ),
                    Err(error) => self.finish_workspace_file_restore(
                        tab_id,
                        Err(WorkspaceRestoreFailure::ResourceFailure),
                        error.public_message().into(),
                        cx,
                    ),
                }
            }
            WorkspaceSessionTab::SftpTransfer { .. } => {
                self.restore_sftp_transfer_tab(tab_id, tab, window, cx);
            }
            WorkspaceSessionTab::Unsupported { kind, .. } => {
                self.finish_workspace_file_restore(
                    tab_id,
                    Err(WorkspaceRestoreFailure::UnsupportedTab),
                    format!(
                        "{}{}{}",
                        crate::locale::text(
                            "当前版本无法恢复工作区标签类型“",
                            "This version cannot restore workspace tab type \"",
                        ),
                        kind,
                        crate::locale::text(
                            "”；原快照数据已保留。",
                            "\"; the original snapshot was kept.",
                        ),
                    )
                    .into(),
                    cx,
                );
            }
            WorkspaceSessionTab::Terminal { .. } => {}
        }
    }

    /// 失败项只有经用户显式触发才重跑，避免渲染循环反复连接无效目标。
    pub(super) fn retry_workspace_file_tab(
        &mut self,
        tab_id: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspace_files.retry_failed_tab(tab_id) {
            self.ensure_workspace_file_tab(tab_id, window, cx);
        }
    }

    /// 只有 Root 完成 editor/传输/外部 lease 清理后才能移除 TabId 的运行态资源。
    /// 同时依据 reducer 的关闭后快照校准文件来源，失败或取消不会走这里，也不影响共享工作台。
    ///
    /// @author kongweiguang
    pub(super) fn remove_workspace_file_tab_after_close(&mut self, tab_id: &TabId) {
        self.workspace_files.tabs.remove(tab_id);
        self.files_workspace_selection.reconcile_removed_tab(
            tab_id,
            self.workspace_controller
                .as_ref()
                .map(|controller| controller.snapshot()),
        );
    }

    /// 激活共享编辑器中该 workspace 文件的 endpoint，readonly aliases 仍使用最严格策略。
    pub(super) fn activate_workspace_file_tab(&mut self, tab_id: &TabId, cx: &mut Context<Self>) {
        let Some(runtime) = self.workspace_files.tabs.get(tab_id) else {
            return;
        };
        let Some(endpoint) = runtime.endpoint.clone() else {
            return;
        };
        let WorkspaceSessionTab::WorkspaceFile { title, .. } = &runtime.tab else {
            return;
        };
        let readonly = self
            .workspace_files
            .workspace_file_requires_readonly(&endpoint);
        if let Some(editor) = self.file_editor_workspace.clone() {
            let result = editor.update(cx, |workspace, cx| {
                workspace.open_workspace_file(endpoint, title.clone(), readonly, cx)
            });
            if let Err(error) = result {
                self.status = Some(error.public_message().into());
                cx.notify();
                return;
            }
            self.center_panel = CenterPanel::Editor;
            cx.notify();
        }
    }

    /// 把业务结果同步到 controller 与 tab runtime；恢复失败不修改持久化 workspace snapshot。
    ///
    /// @author kongweiguang
    fn finish_workspace_file_restore(
        &mut self,
        tab_id: &TabId,
        result: Result<Option<FileEndpoint>, WorkspaceRestoreFailure>,
        message: SharedString,
        cx: &mut Context<Self>,
    ) {
        let controller_result = result.as_ref().map(|_| ()).map_err(|failure| *failure);
        self.workspace_files
            .set_result(tab_id, result.map_err(|failure| (failure, message.clone())));
        if let Some(controller) = self.workspace_controller.as_mut()
            && let Err(error) = controller.record_restore_result(
                WorkspaceRestoreItemId::Tab(tab_id.clone()),
                controller_result,
            )
        {
            self.status = Some(error.public_message().into());
        }
        if !message.is_empty() {
            self.status = Some(message);
        }
        self.refresh_workspace_views(cx);
        cx.notify();
    }

    /// 恢复完整双面板 SFTP 布局；临时 external launch 仍保持明确不可恢复状态。
    ///
    /// @author kongweiguang
    fn restore_sftp_transfer_tab(
        &mut self,
        tab_id: &TabId,
        tab: WorkspaceSessionTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let local_root = match std::env::current_dir() {
            Ok(path) => path,
            Err(_) => {
                self.finish_workspace_file_restore(
                    tab_id,
                    Err(WorkspaceRestoreFailure::ResourceFailure),
                    crate::locale::text(
                        "无法确定本地文件目录，SFTP 工作区未恢复。",
                        "The local file directory is unavailable; the SFTP workspace was not restored.",
                    ),
                    cx,
                );
                return;
            }
        };
        let endpoints = match sftp_transfer_endpoints(&tab, &local_root) {
            Ok(endpoints) => endpoints,
            Err(issue) => {
                self.finish_workspace_file_restore(
                    tab_id,
                    Err(issue.restore_failure()),
                    issue.message(),
                    cx,
                );
                return;
            }
        };
        let (Some(files), Some(transfers)) =
            (self.services.files.clone(), self.services.transfers.clone())
        else {
            self.finish_workspace_file_restore(
                tab_id,
                Err(WorkspaceRestoreFailure::ResourceFailure),
                crate::locale::text(
                    "文件或传输服务尚未配置，SFTP 工作区未恢复。",
                    "The file or transfer service is unavailable; the SFTP workspace was not restored.",
                ),
                cx,
            );
            return;
        };

        let executor = self.services.executor.clone();
        let right_endpoint = endpoints.right.clone();
        let target_name_lookup = crate::features::files::FileTargetNameLookup::new(
            self.services.hosts.clone(),
            self.services.operations.clone(),
        );
        let pane = cx.new(|_| {
            FilesPane::new(
                files,
                transfers,
                executor,
                target_name_lookup,
                right_endpoint.clone(),
            )
        });
        let open_result = pane.update(cx, |files, cx| {
            files.open_workspace_transfer(
                endpoints.left.clone(),
                right_endpoint,
                endpoints.locked_left_host_id.clone(),
                endpoints.initial_right_selection.clone(),
                cx,
            )
        });
        if let Err(error) = open_result {
            self.finish_workspace_file_restore(
                tab_id,
                Err(WorkspaceRestoreFailure::ResourceFailure),
                error.public_message().into(),
                cx,
            );
            return;
        }
        let subscriptions = Self::subscribe_files_pane_events(&pane, window, cx);
        self.workspace_files
            .set_files_pane(tab_id, pane, subscriptions);
        self.finish_workspace_file_restore(tab_id, Ok(None), SharedString::default(), cx);
    }
}

impl RootView {
    /// 文件标签动作只接受当前快照与已 Ready runtime 对应的精确 endpoint；成功不占用根层错误栏。
    ///
    /// @author kongweiguang
    pub(super) fn handle_workspace_file_tab_action(
        &mut self,
        tab_id: &TabId,
        action: WorkspaceFileTabAction,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot_tab = self.workspace_controller.as_ref().and_then(|controller| {
            controller
                .snapshot()
                .terminal_tabs
                .iter()
                .find(|tab| tab.id() == tab_id)
                .cloned()
        });
        let Some(endpoint) =
            workspace_file_action_endpoint(tab_id, snapshot_tab.as_ref(), &self.workspace_files)
        else {
            self.status = Some(crate::locale::text(
                "文件标签目标已失效或尚未恢复；请重新打开该文件标签后重试。",
                "The file tab target is stale or not ready. Reopen the file tab and try again.",
            ));
            cx.notify();
            return;
        };

        match action {
            WorkspaceFileTabAction::CopyFullPath => {
                let Some(path) = workspace_file_full_path(&endpoint) else {
                    self.status = Some(crate::locale::text(
                        "此本地路径无法无损转换为剪贴板文本。",
                        "This local path cannot be represented losslessly as clipboard text.",
                    ));
                    cx.notify();
                    return;
                };
                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(path));
                self.status = None;
                cx.notify();
            }
            WorkspaceFileTabAction::Reload => {
                let Some(editor) = self.file_editor_workspace.clone() else {
                    self.status = Some(crate::locale::text(
                        "文件编辑器当前不可用。",
                        "The file editor is unavailable.",
                    ));
                    cx.notify();
                    return;
                };
                if !editor_projection_contains_endpoint(
                    &editor.read(cx).workspace_projection(),
                    &endpoint,
                ) {
                    self.status = Some(crate::locale::text(
                        "该文件没有打开的编辑标签；请先在文件面板中打开文件。",
                        "This file is not open in the editor. Open it from the Files panel first.",
                    ));
                    cx.notify();
                    return;
                }
                let accepted = editor.update(cx, |workspace, cx| {
                    workspace.request_user_reload(endpoint.clone(), None, cx)
                });
                self.status = (!accepted).then(|| {
                    crate::locale::text(
                        "编辑器正忙或已有确认未完成；请先完成当前操作。",
                        "The editor is busy or another confirmation is pending. Finish that action first.",
                    )
                });
                cx.notify();
            }
        }
    }
}

/// 将已冻结的 WorkspaceFile target 映射到 FileService endpoint；先保留损坏目标的原始原因，再校验来源协议。
///
/// @author kongweiguang
fn workspace_file_endpoint_parts(
    target: &WorkspaceTargetRef,
    path: &str,
    source: kerminal_application::workspace::WorkspaceFileSource,
) -> Result<FileEndpoint, WorkspaceFileRestoreIssue> {
    if path.trim().is_empty() || path.chars().any(char::is_control) {
        return Err(WorkspaceFileRestoreIssue::InvalidPath);
    }
    // 解码器已经识别出目标不可用的原因；协议不匹配检查不能覆盖这条更具体的失效信息。
    if let WorkspaceTargetRef::Unavailable { reason, .. } = target {
        return Err(WorkspaceFileRestoreIssue::Unavailable(*reason));
    }
    match (source, target) {
        (
            kerminal_application::workspace::WorkspaceFileSource::Sftp,
            WorkspaceTargetRef::Ssh { host_id },
        ) => ensure_persisted_host(host_id)?,
        (
            kerminal_application::workspace::WorkspaceFileSource::Container,
            WorkspaceTargetRef::DockerContainer { host_id, .. },
        ) => ensure_persisted_host(host_id)?,
        (kerminal_application::workspace::WorkspaceFileSource::Sftp, _)
        | (kerminal_application::workspace::WorkspaceFileSource::Container, _) => {
            return Err(WorkspaceFileRestoreIssue::UnsupportedTarget);
        }
        _ => {}
    }

    match target {
        WorkspaceTargetRef::Local { .. } => Ok(FileEndpoint::Local { path: path.into() }),
        WorkspaceTargetRef::Ssh { host_id } => {
            ensure_persisted_host(host_id)?;
            Ok(FileEndpoint::Remote {
                host_id: host_id.clone(),
                path: RemotePath::parse(path)
                    .map_err(|_| WorkspaceFileRestoreIssue::InvalidPath)?,
            })
        }
        WorkspaceTargetRef::DockerContainer {
            host_id,
            container_id,
            runtime,
            ..
        } => {
            ensure_persisted_host(host_id)?;
            if container_id.trim().is_empty() {
                return Err(WorkspaceFileRestoreIssue::MissingTarget);
            }
            Ok(FileEndpoint::Container {
                host_id: Some(host_id.clone()),
                runtime: match runtime {
                    kerminal_application::workspace::WorkspaceContainerRuntime::Docker => {
                        ContainerRuntime::Docker
                    }
                    kerminal_application::workspace::WorkspaceContainerRuntime::Podman => {
                        ContainerRuntime::Podman
                    }
                },
                container_id: container_id.clone(),
                path: RemotePath::parse(path)
                    .map_err(|_| WorkspaceFileRestoreIssue::InvalidPath)?,
            })
        }
        WorkspaceTargetRef::Telnet { .. } | WorkspaceTargetRef::Serial { .. } => {
            Err(WorkspaceFileRestoreIssue::UnsupportedTarget)
        }
        WorkspaceTargetRef::Unavailable { reason, .. } => {
            Err(WorkspaceFileRestoreIssue::Unavailable(*reason))
        }
    }
}

/// 按可选左右起始路径构造端点；旧快照缺少左路径时保留原有本地工作区根目录语义。
///
/// @author kongweiguang
fn sftp_transfer_endpoints(
    tab: &WorkspaceSessionTab,
    local_root: &Path,
) -> Result<SftpTransferEndpoints, WorkspaceFileRestoreIssue> {
    let WorkspaceSessionTab::SftpTransfer {
        left_host_id,
        locked_left_host_id,
        right_host_id,
        external_launch_id,
        initial_left_path,
        initial_right_path,
        initial_right_selection,
        ..
    } = tab
    else {
        return Err(WorkspaceFileRestoreIssue::UnsupportedTarget);
    };
    if external_launch_id.is_some() {
        return Err(WorkspaceFileRestoreIssue::ExternalTarget);
    }
    if [initial_left_path.as_deref(), initial_right_path.as_deref()]
        .into_iter()
        .flatten()
        .any(|path| path.chars().any(char::is_control))
    {
        return Err(WorkspaceFileRestoreIssue::InvalidPath);
    }
    let left_host_id = locked_left_host_id.as_ref().or(left_host_id.as_ref());
    if let Some(host_id) = left_host_id {
        ensure_persisted_host(host_id)?;
    }
    if let Some(host_id) = right_host_id {
        ensure_persisted_host(host_id)?;
    }
    let left_path = initial_left_path
        .as_deref()
        .filter(|path| !path.trim().is_empty());
    let left = match left_host_id {
        Some(host_id) => FileEndpoint::Remote {
            host_id: host_id.clone(),
            path: RemotePath::parse(left_path.unwrap_or("/"))
                .map_err(|_| WorkspaceFileRestoreIssue::InvalidPath)?,
        },
        None => FileEndpoint::Local {
            path: left_path
                .map(Path::new)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| local_root.to_path_buf()),
        },
    };
    let right = match right_host_id {
        Some(host_id) => FileEndpoint::Remote {
            host_id: host_id.clone(),
            path: RemotePath::parse(initial_right_path.as_deref().unwrap_or("/"))
                .map_err(|_| WorkspaceFileRestoreIssue::InvalidPath)?,
        },
        None => FileEndpoint::Local {
            path: initial_right_path
                .as_deref()
                .filter(|path| !path.trim().is_empty())
                .map(Path::new)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| local_root.to_path_buf()),
        },
    };
    Ok(SftpTransferEndpoints {
        left,
        right,
        locked_left_host_id: locked_left_host_id.clone(),
        initial_right_selection: initial_right_selection.clone(),
    })
}

/// 临时 external HostId 在进程退出后没有可解析凭据，禁止按同名普通主机尝试重连。
fn ensure_persisted_host(host_id: &HostId) -> Result<(), WorkspaceFileRestoreIssue> {
    if host_id.as_str().starts_with("external:") {
        Err(WorkspaceFileRestoreIssue::ExternalTarget)
    } else {
        Ok(())
    }
}

/// 构造可恢复的本地到指定 SSH 主机标签；主机默认本地目录随标签保存以避免重启后丢失。
///
/// @author kongweiguang
pub(super) fn sftp_transfer_tab_for_host(
    id: TabId,
    host_id: HostId,
    initial_left_path: Option<String>,
    initial_right_path: &RemotePath,
    title: &str,
) -> WorkspaceSessionTab {
    WorkspaceSessionTab::SftpTransfer {
        id,
        tab_group_id: None,
        title: title.to_owned(),
        machine_id: host_id.as_str().to_owned(),
        left_host_id: None,
        locked_left_host_id: None,
        right_host_id: Some(host_id),
        external_launch_id: None,
        initial_left_path,
        initial_right_path: Some(initial_right_path.as_str().to_owned()),
        initial_right_selection: None,
    }
}

/// 只允许 Files 激活仍存在于当前 Editor 工作区的 endpoint，拦截关闭后的迟到点击。
///
/// @author kongweiguang
fn editor_projection_contains_endpoint(
    projection: &EditorWorkspaceProjection,
    endpoint: &FileEndpoint,
) -> bool {
    projection.tabs.iter().any(|tab| &tab.endpoint == endpoint)
}

/// 只有快照、恢复 descriptor 和 Ready runtime 三者都指向同一 TabId/endpoint 才允许菜单操作。
///
/// @author kongweiguang
fn workspace_file_action_endpoint(
    tab_id: &TabId,
    snapshot_tab: Option<&WorkspaceSessionTab>,
    runtime: &WorkspaceFileRuntime,
) -> Option<FileEndpoint> {
    let WorkspaceSessionTab::WorkspaceFile {
        id,
        target,
        path,
        source,
        ..
    } = snapshot_tab?
    else {
        return None;
    };
    if id != tab_id {
        return None;
    }
    let runtime_tab = runtime.tabs.get(tab_id)?;
    if runtime_tab.status != WorkspaceFileRestoreState::Ready || runtime_tab.tab.id() != tab_id {
        return None;
    }
    let endpoint = workspace_file_endpoint_parts(target, path, *source).ok()?;
    (runtime_tab.endpoint.as_ref() == Some(&endpoint)).then_some(endpoint)
}

/// 剪贴板只复制文件路径，不附加 host/container 身份；非 UTF-8 本地路径拒绝有损复制。
///
/// @author kongweiguang
fn workspace_file_full_path(endpoint: &FileEndpoint) -> Option<String> {
    match endpoint {
        FileEndpoint::Local { path } => path.to_str().map(str::to_owned),
        FileEndpoint::Remote { path, .. } | FileEndpoint::Container { path, .. } => {
            Some(path.as_str().to_owned())
        }
    }
}

/// 空编辑器投影不是导航指令；仅在当前临时编辑页已耗尽且原选中项仍为终端时恢复视图。
///
/// @author kongweiguang
fn should_return_to_terminal_after_editor_close(
    center_panel: CenterPanel,
    projection: &EditorWorkspaceProjection,
    active_tab_is_terminal: bool,
) -> bool {
    center_panel == CenterPanel::Editor && projection.tabs.is_empty() && active_tab_is_terminal
}

impl WorkspaceFileRestoreIssue {
    /// 将文件恢复失败转换成 workspace reducer 使用的稳定业务分类。
    fn restore_failure(self) -> WorkspaceRestoreFailure {
        match self {
            Self::MissingTarget | Self::ExternalTarget => WorkspaceRestoreFailure::MissingTarget,
            Self::Unavailable(reason) => WorkspaceRestoreFailure::UnavailableTarget(reason),
            Self::UnsupportedTarget => WorkspaceRestoreFailure::UnsupportedTab,
            Self::InvalidPath => WorkspaceRestoreFailure::UnavailableTarget(
                WorkspaceTargetUnavailableReason::Malformed,
            ),
        }
    }

    /// 将协议、路径和临时目标错误映射为脱敏的中英双语反馈。
    ///
    /// @author kongweiguang
    fn message(self) -> SharedString {
        match self {
            Self::MissingTarget => crate::locale::text(
                "工作区目标缺失或已失效；未切换到其他主机。",
                "The workspace target is missing or unavailable. No other host was selected.",
            ),
            Self::UnsupportedTarget => crate::locale::text(
                "此文件目标不支持当前文件服务协议。",
                "This file target is not supported by the configured file service.",
            ),
            Self::InvalidPath => crate::locale::text(
                "工作区文件路径无效；原快照数据已保留。",
                "The workspace file path is invalid. The original snapshot was kept.",
            ),
            Self::ExternalTarget => crate::locale::text(
                "此临时外部连接不能跨应用重启恢复；原标签仍保留。",
                "This temporary external connection cannot be restored after restart. The original tab was kept.",
            ),
            Self::Unavailable(reason) => crate::locale::text(
                match reason {
                    WorkspaceTargetUnavailableReason::Malformed
                    | WorkspaceTargetUnavailableReason::UnknownKind
                    | WorkspaceTargetUnavailableReason::InvalidHostId
                    | WorkspaceTargetUnavailableReason::MissingContainerId
                    | WorkspaceTargetUnavailableReason::UnknownRuntime => {
                        "工作区目标无法安全恢复；原快照数据已保留。"
                    }
                },
                "The workspace target could not be restored safely; the original snapshot was kept.",
            ),
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_workspace_files.rs"]
mod tests;
