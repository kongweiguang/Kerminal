//! 按稳定 TabId 恢复非终端工作区资源，避免不同文件面板共享可变目录状态。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};

use gpui_kit::{App, AppContext as _, Context, Entity, SharedString, Subscription, Window};
use kerminal_application::{
    AppError, AppResult,
    platform::external_launch::ExternalTargetLease,
    preferences::ToolRailToolId,
    workspace::{
        WorkspaceFileAccess, WorkspaceSessionSnapshot, WorkspaceSessionTab, WorkspaceTargetRef,
        WorkspaceTargetUnavailableReason,
    },
};
use kerminal_domain::{
    files::{ContainerRuntime, FileEndpoint, RemotePath},
    identity::{HostId, TabId},
};

use crate::features::{
    editor::{EditorWorkspaceProjection, FileEditorWorkspace},
    files::{
        ActivateEditorFileRequested, CreateSftpHostRequested, FilesPane, FilesPaneEvent,
        OpenFileRequested, SftpHostKeyTrustRequested, SftpHostRequested,
    },
    workspace_controller::{WorkspaceController, WorkspaceRestoreFailure, WorkspaceRestoreItemId},
    workspace_tabs::WorkspaceFileTabAction,
};
use tokio_util::sync::CancellationToken;

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
    external_target_lease: Option<ExternalTargetLease>,
    transient: bool,
    /// 保存门禁占用时保留最新的双侧意图，不让目录已经变化却悄悄丢失恢复位置。
    pending_transfer_endpoints: Option<(FileEndpoint, Option<FileEndpoint>)>,
}

impl WorkspaceFileTabRuntime {
    /// 有真实运行态端点时只比较该端点；尚未装配时才解析可恢复描述，避免临时目标被误当成本机文件。
    ///
    /// @author kongweiguang
    fn matches_file_endpoint(&self, endpoint: &FileEndpoint) -> bool {
        let WorkspaceSessionTab::WorkspaceFile {
            target,
            path,
            source,
            ..
        } = &self.tab
        else {
            return false;
        };
        match self.endpoint.as_ref() {
            Some(current) => current == endpoint,
            None => workspace_file_endpoint_parts(target, path, *source)
                .is_ok_and(|current| &current == endpoint),
        }
    }
}

/// 一个 FilesPane 的 Root 桥接和删除观察器同生共灭，避免工作区标签漏接安全边界。
pub(super) struct FilesPaneSubscriptions {
    _open_file: Subscription,
    _activate_editor_file: Subscription,
    _sftp_host: Subscription,
    _sftp_host_key_trust: Subscription,
    _create_sftp_host: Subscription,
    _delete_confirmation_observer: Subscription,
    _pane_events: Subscription,
    host_key_trust_tasks: FilesPaneTaskRegistry,
}

type FilesPaneTaskRegistry = Arc<Mutex<HashMap<u64, CancellationToken>>>;

impl Drop for FilesPaneSubscriptions {
    /// 释放桥接时取消该面板的未提交信任；已开始写入仍须等待原端点回读。
    fn drop(&mut self) {
        for cancellation in lock_files_pane_tasks(&self.host_key_trust_tasks).values() {
            cancellation.cancel();
        }
    }
}

/// 短暂锁住每个 pane 的请求表；锁中不等待传输，也从不持有 GPUI 更新回调。
///
/// @author kongweiguang
fn lock_files_pane_tasks(
    tasks: &FilesPaneTaskRegistry,
) -> MutexGuard<'_, HashMap<u64, CancellationToken>> {
    tasks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    pub right: Option<FileEndpoint>,
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
                    external_target_lease: None,
                    transient: false,
                    pending_transfer_endpoints: None,
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
        if !matches!(
            &tab,
            WorkspaceSessionTab::SftpTransfer {
                external_launch_id: None,
                ..
            }
        ) {
            return false;
        }
        self.register_pending_file_tab(tab)
    }

    /// Compose 入口只登记明确只读的文件描述，实际加载和失败重试复用恢复链路。
    ///
    /// @author kongweiguang
    pub(super) fn register_pending_compose_yaml_tab(&mut self, tab: WorkspaceSessionTab) -> bool {
        if !matches!(
            &tab,
            WorkspaceSessionTab::WorkspaceFile {
                access: kerminal_application::workspace::WorkspaceFileAccess::Readonly,
                source: kerminal_application::workspace::WorkspaceFileSource::ComposeYaml,
                ..
            }
        ) {
            return false;
        }
        self.register_pending_file_tab(tab)
    }

    /// 登记与快照提交分开，允许 Root 在 CAS 失败时撤销本轮描述而不重建已有文件面板。
    ///
    /// @author kongweiguang
    fn register_pending_file_tab(&mut self, tab: WorkspaceSessionTab) -> bool {
        if !self.snapshot_installed || self.tabs.contains_key(tab.id()) {
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
                external_target_lease: None,
                transient: false,
                pending_transfer_endpoints: None,
            },
        );
        true
    }

    /// 活动文件沿用已验证的端点；临时目标由独立 lease 保活，持久恢复仍走严格 descriptor 校验。
    ///
    /// @author kongweiguang
    pub(super) fn register_open_file_tab(
        &mut self,
        tab: WorkspaceSessionTab,
        endpoint: FileEndpoint,
        external_target_lease: Option<ExternalTargetLease>,
        transient: bool,
    ) -> bool {
        let id = tab.id().clone();
        if !self.register_pending_file_tab(tab) {
            return false;
        }
        if let Some(runtime) = self.tabs.get_mut(&id) {
            runtime.endpoint = Some(endpoint);
            runtime.external_target_lease = external_target_lease;
            runtime.transient = transient;
        }
        true
    }

    /// 完整 endpoint 决定导航身份；已恢复、待恢复和失败描述都不能按来源创建新的别名。
    ///
    /// @author kongweiguang
    pub(super) fn tab_for_endpoint(&self, endpoint: &FileEndpoint) -> Option<TabId> {
        self.tabs
            .iter()
            .find_map(|(id, runtime)| runtime.matches_file_endpoint(endpoint).then(|| id.clone()))
    }

    /// 把同一标签收紧为只读；编辑器与快照在单次更新内协调，失败恢复原策略且不改变正文。
    ///
    /// @author kongweiguang
    pub(super) fn restrict_file_to_readonly(
        &mut self,
        controller: &mut WorkspaceController,
        editor: &Entity<FileEditorWorkspace>,
        tab_id: &TabId,
        endpoint: &FileEndpoint,
        cx: &mut App,
    ) -> AppResult<bool> {
        let runtime = self
            .tabs
            .get_mut(tab_id)
            .ok_or_else(|| AppError::not_found("工作区文件标签不存在"))?;
        if !runtime.matches_file_endpoint(endpoint) {
            return Err(AppError::conflict("工作区文件目标已变化"));
        }
        let WorkspaceSessionTab::WorkspaceFile {
            target: runtime_target,
            path: runtime_path,
            source: runtime_source,
            access: runtime_access,
            ..
        } = &mut runtime.tab
        else {
            return Err(AppError::conflict("工作区标签不是文件"));
        };
        let original = controller.snapshot().clone();
        let mut candidate = original.clone();
        let Some(WorkspaceSessionTab::WorkspaceFile {
            target,
            path,
            source,
            access,
            ..
        }) = candidate
            .terminal_tabs
            .iter_mut()
            .find(|tab| tab.id() == tab_id)
        else {
            return Err(AppError::conflict("工作区文件描述已变化"));
        };
        if target != runtime_target
            || path != runtime_path
            || source != runtime_source
            || access != runtime_access
        {
            return Err(AppError::conflict("工作区文件描述与运行状态不一致"));
        }
        let changed = *access != WorkspaceFileAccess::Readonly;
        *access = WorkspaceFileAccess::Readonly;

        // 先检查 controller 的资源和保存门禁；随后编辑器事务不会产生并发 Root 回调。
        controller.synchronize_snapshot(original)?;
        editor.update(cx, |workspace, cx| {
            let old_policy = workspace.workspace_file_readonly_before_restriction(endpoint, cx)?;
            if old_policy == Some(false) {
                workspace.set_workspace_file_readonly(endpoint, true, cx)?;
            }
            if let Err(error) = controller.synchronize_snapshot(candidate) {
                if old_policy == Some(false) {
                    workspace
                        .set_workspace_file_readonly(endpoint, false, cx)
                        .map_err(|_| AppError::conflict("工作区文件只读状态回滚失败"))?;
                }
                return Err(error);
            }
            *runtime_access = WorkspaceFileAccess::Readonly;
            Ok(changed)
        })
    }

    /// 仅把已经装配成功的文件别名交给关闭协调，尚未恢复的标签不能被空投影误删。
    ///
    /// @author kongweiguang
    pub(super) fn ready_file_endpoints(&self) -> Vec<(TabId, FileEndpoint)> {
        self.tabs
            .iter()
            .filter_map(|(id, runtime)| {
                if runtime.status != WorkspaceFileRestoreState::Ready
                    || !matches!(runtime.tab, WorkspaceSessionTab::WorkspaceFile { .. })
                {
                    return None;
                }
                runtime
                    .endpoint
                    .clone()
                    .map(|endpoint| (id.clone(), endpoint))
            })
            .collect()
    }

    /// 本机容器没有可恢复的宿主身份；与 external 文件一起从唯一持久化投影排除。
    ///
    /// @author kongweiguang
    pub(super) fn transient_tab_ids(&self) -> BTreeSet<TabId> {
        self.tabs
            .iter()
            .filter(|(_, runtime)| runtime.transient)
            .map(|(id, _)| id.clone())
            .collect()
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

    /// 每个独立传输标签拥有不同 FilesPane；通过实体身份定位，不能使用后台活动标签猜测。
    ///
    /// @author kongweiguang
    fn transfer_tab_for_pane(&self, pane: &Entity<FilesPane>) -> Option<TabId> {
        self.tabs.iter().find_map(|(id, runtime)| {
            (matches!(runtime.tab, WorkspaceSessionTab::SftpTransfer { .. })
                && runtime
                    .files_pane
                    .as_ref()
                    .is_some_and(|current| current.entity_id() == pane.entity_id()))
            .then(|| id.clone())
        })
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

    /// 文件 Tab 为右栏提供父目录及自身连接所有权，不借用已关闭的来源终端或重建 session binding。
    /// 紧凑浏览器仅支持本机与 SFTP；容器仍使用专用浏览入口，临时目标必须持有精确匹配的 lease。
    ///
    /// @author kongweiguang
    pub(super) fn file_browser_target(
        &self,
        tab_id: &TabId,
    ) -> Option<(FileEndpoint, Option<ExternalTargetLease>)> {
        let runtime = self.tabs.get(tab_id)?;
        if !matches!(runtime.tab, WorkspaceSessionTab::WorkspaceFile { .. }) {
            return None;
        }
        let endpoint = runtime.endpoint.as_ref()?;
        match endpoint {
            FileEndpoint::Local { .. } if runtime.external_target_lease.is_none() => {}
            FileEndpoint::Remote { host_id, .. } => {
                let lease = runtime.external_target_lease.as_ref();
                if lease.is_some_and(|lease| lease.host_id() != host_id)
                    || (host_id.as_str().starts_with("external:") && lease.is_none())
                {
                    return None;
                }
            }
            FileEndpoint::Local { .. } | FileEndpoint::Container { .. } => return None,
        }
        Some((
            crate::features::files::parent_endpoint(endpoint)?,
            runtime.external_target_lease.clone(),
        ))
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

    /// 多个 tab 共享同一 endpoint 时只读策略取最严格值，且与导航使用相同的端点识别边界。
    ///
    /// @author kongweiguang
    pub fn workspace_file_requires_readonly(&self, endpoint: &FileEndpoint) -> bool {
        self.tabs.values().any(|tab| {
            let WorkspaceSessionTab::WorkspaceFile { access, .. } = &tab.tab else {
                return false;
            };
            *access == WorkspaceFileAccess::Readonly && tab.matches_file_endpoint(endpoint)
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

    /// 只有显式重试才能重新装配；临时文件必须保留不可由持久 descriptor 重建的原始端点。
    pub fn retry_failed_tab(&mut self, tab_id: &TabId) -> bool {
        let Some(tab) = self.tabs.get_mut(tab_id) else {
            return false;
        };
        if !matches!(tab.status, WorkspaceFileRestoreState::Failed { .. }) {
            return false;
        }
        tab.status = WorkspaceFileRestoreState::Pending;
        if !tab.transient {
            tab.endpoint = None;
        }
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
    /// 文件面板上报的是完整双端点；共享工作台不属于可持久标签，因此只同步对应的独立实例。
    ///
    /// @author kongweiguang
    fn sync_sftp_transfer_endpoints(
        &mut self,
        pane: &Entity<FilesPane>,
        left: &FileEndpoint,
        right: &Option<FileEndpoint>,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_id) = self.workspace_files.transfer_tab_for_pane(pane) else {
            return;
        };
        let Some(runtime) = self.workspace_files.tabs.get_mut(&tab_id) else {
            return;
        };
        let endpoints = (left.clone(), right.clone());
        if runtime.pending_transfer_endpoints.as_ref() == Some(&endpoints) {
            return;
        }
        runtime.pending_transfer_endpoints = Some(endpoints);
        if self.flush_pending_sftp_transfer_endpoints(cx) {
            self.refresh_workspace_views(cx);
            self.persist_workspace_snapshot(cx);
        }
    }

    /// 只在 controller 允许普通变更时同步待保存端点；关闭标签后不会重建描述或重试未知提交。
    ///
    /// @author kongweiguang
    pub(super) fn flush_pending_sftp_transfer_endpoints(&mut self, cx: &mut Context<Self>) -> bool {
        let pending = self
            .workspace_files
            .tabs
            .iter()
            .filter_map(|(tab_id, runtime)| {
                (runtime.files_pane.is_some()
                    && matches!(runtime.tab, WorkspaceSessionTab::SftpTransfer { .. }))
                .then(|| {
                    runtime
                        .pending_transfer_endpoints
                        .clone()
                        .map(|endpoints| (tab_id.clone(), endpoints))
                })
                .flatten()
            })
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return false;
        }
        let Some(controller) = self.workspace_controller.as_mut() else {
            self.status = Some(crate::locale::text(
                "文件目录已更改，但工作区未就绪，当前位置尚未保存。",
                "File directories changed, but the workspace is unavailable. Their positions are not saved yet.",
            ));
            cx.notify();
            return false;
        };
        let mut snapshot = controller.snapshot().clone();
        let mut changed_tabs = Vec::new();
        for (tab_id, (left, right)) in &pending {
            let Some(tab) = snapshot
                .terminal_tabs
                .iter_mut()
                .find(|tab| tab.id() == tab_id)
            else {
                continue;
            };
            match update_sftp_transfer_descriptor(tab, left, right) {
                Ok(()) => changed_tabs.push((tab_id.clone(), tab.clone())),
                Err(error) => {
                    self.status = Some(crate::locale::text(
                        format!("当前位置尚未保存：{}", error.public_message()),
                        format!(
                            "The current position is not saved: {}",
                            error.public_message()
                        ),
                    ));
                    cx.notify();
                    return false;
                }
            }
        }
        if changed_tabs.is_empty() {
            return false;
        }
        let changed = match controller.synchronize_snapshot(snapshot) {
            Ok(crate::features::workspace_controller::WorkspaceControllerEffect::Changed {
                ..
            }) => true,
            Ok(crate::features::workspace_controller::WorkspaceControllerEffect::Unchanged) => {
                false
            }
            Ok(
                crate::features::workspace_controller::WorkspaceControllerEffect::ResourceOperation(
                    _,
                ),
            ) => return false,
            Err(error) => {
                self.status = Some(crate::locale::text(
                    format!(
                        "文件目录已更改，当前位置尚未保存：{}",
                        error.public_message()
                    ),
                    format!(
                        "File directories changed but are not saved yet: {}",
                        error.public_message()
                    ),
                ));
                cx.notify();
                return false;
            }
        };
        for (tab_id, tab) in changed_tabs {
            if let Some(runtime) = self.workspace_files.tabs.get_mut(&tab_id) {
                runtime.tab = tab;
                runtime.pending_transfer_endpoints = None;
            }
        }
        changed
    }

    /// 为每个文件视图绑定同一组 typed Root 桥接；缓存中的右栏面板不能回投导航或开启根确认。
    /// 主机选择复用连接预检并绑定发起的共享工作台或独立标签，不让文件视图自行建连接。
    /// 新建主机同样交给 Root 捕获来源身份，保存成功不依赖当时的后台活动传输标签。
    /// 仅当前紧凑 SFTP 抽屉的成功文件导航收起目标标签遮罩，目录、失败及其它来源保留原视图。
    ///
    /// @author kongweiguang
    pub(super) fn subscribe_files_pane_events(
        pane: &Entity<FilesPane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> FilesPaneSubscriptions {
        let host_key_trust_tasks = Arc::new(Mutex::new(HashMap::new()));
        let event_trust_tasks = host_key_trust_tasks.clone();
        let open_file = cx.subscribe_in(
            pane,
            window,
            |root, source, event: &OpenFileRequested, window, cx| {
                if root.is_cached_sftp_panel(source) {
                    return;
                }
                let from_compact_sftp = root.compact_tool_drawer_open
                    && root.tool_rail_rendered_compact == (true, Some(ToolRailToolId::Sftp))
                    && root.sftp_panel.as_ref().is_some_and(|pane| pane == source);
                let accepted = root.open_file_endpoint_with_lease(
                    event.endpoint.clone(),
                    event.external_target_lease.clone(),
                    window,
                    cx,
                );
                if accepted && from_compact_sftp {
                    root.reveal_file_from_compact_sftp(&event.endpoint, window, cx);
                }
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
            |root, source, event: &SftpHostRequested, _, cx| {
                root.request_file_sftp_host(
                    source,
                    event.side,
                    event.generation,
                    event.host_id.clone(),
                    cx,
                );
            },
        );
        let create_sftp_host = cx.subscribe_in(
            pane,
            window,
            |root, source, event: &CreateSftpHostRequested, window, cx| {
                root.begin_create_sftp_host_from_workbench(
                    source,
                    event.side,
                    event.generation,
                    window,
                    cx,
                );
            },
        );
        let sftp_host_key_trust = cx.subscribe_in(
            pane,
            window,
            move |root, source, event: &SftpHostKeyTrustRequested, _, cx| {
                if root.is_cached_sftp_panel(source)
                    || source.read(cx).current_host_key_trust_request().as_ref() != Some(event)
                {
                    return;
                }
                root.start_file_host_key_trust(
                    source.clone(),
                    event.clone(),
                    event_trust_tasks.clone(),
                    cx,
                );
            },
        );
        let delete_confirmation_observer =
            cx.observe_in(pane, window, |root, source, window, cx| {
                let active = source.read(cx).pending_global_dialog();
                if active && root.is_cached_sftp_panel(&source) {
                    return;
                }
                root.sync_file_delete_root_confirmation(source.clone(), active, window, cx);
            });
        let pane_events = cx.subscribe_in(
            pane,
            window,
            move |root, source, event: &FilesPaneEvent, _, cx| {
                if root.is_cached_sftp_panel(source) {
                    return;
                }
                match event {
                    FilesPaneEvent::SetupTerminalDirectoryFollow { binding } => {
                        root.setup_terminal_directory_follow(source.clone(), binding.clone(), cx);
                    }
                    FilesPaneEvent::TransferEndpointsChanged { left, right } => {
                        root.sync_sftp_transfer_endpoints(source, left, right, cx);
                    }
                    FilesPaneEvent::CancelHostSelection { side, generation } => {
                        root.cancel_file_sftp_selection(source, *side, *generation, cx);
                    }
                }
            },
        );
        FilesPaneSubscriptions {
            _open_file: open_file,
            _activate_editor_file: activate_editor_file,
            _sftp_host: sftp_host,
            _sftp_host_key_trust: sftp_host_key_trust,
            _create_sftp_host: create_sftp_host,
            _delete_confirmation_observer: delete_confirmation_observer,
            _pane_events: pane_events,
            host_key_trust_tasks,
        }
    }

    /// 已接受的文件导航只隐藏目标标签的临时遮罩；旧面板、失败和宽窗不改变工具状态。
    /// 菜单焦点结束后仅向当前编辑器或工作区交接，不能恢复到打开抽屉前的旧终端。
    ///
    /// @author kongweiguang
    fn reveal_file_from_compact_sftp(
        &mut self,
        endpoint: &FileEndpoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_id) = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.snapshot().active_tab_id.clone())
            .filter(|tab_id| {
                self.workspace_files.tab_for_endpoint(endpoint).as_ref() == Some(tab_id)
            })
        else {
            return;
        };
        if self.center_panel != CenterPanel::Editor
            || !self.compact_tool_drawer_open
            || self.tool_rail_rendered_compact != (true, Some(ToolRailToolId::Sftp))
        {
            return;
        }
        let return_to_editor = self
            .compact_drawer_focus_handle
            .contains_focused(window, cx)
            && !self.tool_panel_overlay_focus_is_blocked(window, cx);
        self.tool_rail_state
            .hide_compact_drawer_for_tab(Some(&tab_id));
        self.compact_tool_drawer_open = false;
        self.compact_drawer_return_focus = None;
        if return_to_editor {
            let focused = self.file_editor_workspace.clone().is_some_and(|editor| {
                editor.update(cx, |workspace, cx| {
                    workspace.focus_active_editor(window, cx)
                })
            });
            if !focused {
                window.focus(&self.focus_handle, cx);
            }
        }
        cx.notify();
    }

    /// 只提交文件栏冻结的主机信任意图；应用用例负责逐跳绑定与未知结果回读，回执仍交还原 pane。
    /// 不导航到连接页或打开新终端，避免信任动作改变用户正在浏览的工作区。
    ///
    /// @author kongweiguang
    fn start_file_host_key_trust(
        &mut self,
        source: Entity<FilesPane>,
        request: SftpHostKeyTrustRequested,
        tasks: FilesPaneTaskRegistry,
        cx: &mut Context<Self>,
    ) {
        let Some((hosts, remote)) = self
            .services
            .hosts
            .clone()
            .zip(self.services.remote_connections.clone())
        else {
            source.update(cx, |pane, cx| {
                pane.report_host_key_trust(
                    request.request_id,
                    request.host_id,
                    Err(crate::locale::text(
                        "主机密钥服务不可用。",
                        "Host key services are unavailable.",
                    )
                    .to_string()),
                    cx,
                )
            });
            return;
        };
        let cancellation = CancellationToken::new();
        {
            let mut active = lock_files_pane_tasks(&tasks);
            for previous in active.values() {
                previous.cancel();
            }
            active.clear();
            active.insert(request.request_id, cancellation.clone());
        }
        let executor = self.services.executor.clone();
        cx.spawn(async move |_, cx| {
            let host_id = request.host_id.clone();
            let result = executor
                .run_cancellable(cancellation, move |cancel| async move {
                    kerminal_application::connections::file_host_key::trust_file_host_route_keys(
                        &hosts, remote, host_id, cancel,
                    )
                    .await
                })
                .await
                .map_err(|error| error.public_message().to_owned());
            cx.update(|app| {
                source.update(app, |pane, cx| {
                    pane.report_host_key_trust(request.request_id, request.host_id, result, cx);
                });
            });
            lock_files_pane_tasks(&tasks).remove(&request.request_id);
        })
        .detach();
    }

    /// 安装只使用按钮点击时冻结的 SSH binding；切 Tab 后仍完成原主机请求，不改投当前选择。
    /// @author kongweiguang
    fn setup_terminal_directory_follow(
        &mut self,
        source: Entity<FilesPane>,
        binding: kerminal_application::sessions::UiSessionBinding,
        cx: &mut Context<Self>,
    ) {
        let host_id = self
            .pane_input_handlers
            .get(&binding.session_id)
            .filter(|registration| registration.binding == binding)
            .and_then(|_| match &binding.owner {
                kerminal_domain::sessions::SessionOwner::Ui { pane_id, .. } => {
                    self.workspace_runtime.pane(pane_id)
                }
                _ => None,
            })
            .and_then(|pane| match pane.target.as_ref()? {
                kerminal_domain::sessions::SessionTarget::Ssh(target) => {
                    Some(target.host_id.clone())
                }
                _ => None,
            });
        let Some((operations, host_id)) = self.services.operations.clone().zip(host_id) else {
            source.update(cx, |files, cx| {
                files.report_directory_follow_setup(
                    binding.clone(),
                    Err("终端目标已失效或不是 SSH 终端。".to_owned()),
                    cx,
                )
            });
            return;
        };
        let executor = self.services.executor.clone();
        cx.spawn(async move |_, cx| {
            let result = executor
                .run(move |cancel| async move {
                    operations
                        .configure_terminal_directory_follow(
                            host_id,
                            crate::features::files::build_terminal_directory_follow_setup_script(),
                            cancel,
                        )
                        .await
                })
                .await
                .map_err(|error| error.public_message().to_owned());
            cx.update(|app| {
                source.update(app, |files, cx| {
                    files.report_directory_follow_setup(binding, result, cx)
                })
            });
        })
        .detach();
    }

    /// 将编辑器的当前投影同步到标签导航；最后一个编辑页关闭后只返回原本选中的终端，不创建或改绑会话。
    ///
    /// @author kongweiguang
    pub(super) fn push_editor_workspace_projection(
        &mut self,
        projection: EditorWorkspaceProjection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_editor_navigation(&projection, window, cx);
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
            workspace.activate_open_file(endpoint.clone(), cx)
        });
        if activated {
            if let Some(id) = self.workspace_files.tab_for_endpoint(&endpoint) {
                let event =
                    crate::features::workspace_tabs::WorkspaceTabEvent::Activate(id.clone());
                if self.handle_workspace_tab_event(&event, window, cx) {
                    self.activate_workspace_tab(&id, window, cx);
                }
                return;
            }
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

    /// 延迟创建资源时仅由当前主导航决定活动缓冲；后台恢复不抢导航，失败保留原 descriptor。
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
                let resolved = self
                    .workspace_files
                    .file_endpoint(tab_id)
                    .map(Ok)
                    .unwrap_or_else(|| workspace_file_endpoint_parts(&target, &path, source));
                let endpoint = match resolved {
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
                let activate = self
                    .workspace_controller
                    .as_ref()
                    .is_some_and(|controller| {
                        controller.snapshot().active_tab_id.as_ref() == Some(tab_id)
                    });
                let result = editor.update(cx, |workspace, cx| {
                    workspace.open_workspace_file(endpoint.clone(), title, readonly, activate, cx)
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
                workspace.open_workspace_file(endpoint, title.clone(), readonly, true, cx)
            });
            if let Err(error) = result {
                self.status = Some(error.public_message().into());
                cx.notify();
                return;
            }
            self.cancel_visible_transfer_preparation(cx);
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
        // 空右侧只为实体构造提供内部占位，open_workspace_transfer 不加载该路径。
        let initial_endpoint = right_endpoint
            .clone()
            .unwrap_or_else(|| FileEndpoint::Local {
                path: local_root.clone(),
            });
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
                initial_endpoint,
            )
        });
        let density = super::preferences::current_interface_density(self);
        let open_result = pane.update(cx, |files, cx| {
            files.set_interface_density(density, cx);
            files.open_workspace_transfer(
                endpoints.left.clone(),
                right_endpoint,
                None,
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
        pane.update(cx, |files, cx| files.set_host_picker_enabled(true, cx));
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

/// 持久描述只接受可恢复的本机和已保存 SFTP 目标；临时连接与容器不得借用旧主机身份。
///
/// @author kongweiguang
fn stored_sftp_transfer_endpoint(endpoint: &FileEndpoint) -> AppResult<(Option<HostId>, String)> {
    match endpoint {
        FileEndpoint::Local { path } => {
            let path = path
                .to_str()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| AppError::invalid("本机路径无法无损保存"))?;
            Ok((None, path.to_owned()))
        }
        FileEndpoint::Remote { host_id, path } if !host_id.as_str().starts_with("external:") => {
            Ok((Some(host_id.clone()), path.as_str().to_owned()))
        }
        FileEndpoint::Remote { .. } | FileEndpoint::Container { .. } => {
            Err(AppError::invalid("当前端点不能保存为 SFTP 传输标签"))
        }
    }
}

/// 更新已有 v3 字段并清除旧锁定；标签标题、分组和用户未涉及的快照字段均由原描述持有。
///
/// @author kongweiguang
fn update_sftp_transfer_descriptor(
    tab: &mut WorkspaceSessionTab,
    left: &FileEndpoint,
    right: &Option<FileEndpoint>,
) -> AppResult<()> {
    let (left_host, left_path) = stored_sftp_transfer_endpoint(left)?;
    let (right_host, right_path) = match right {
        Some(endpoint) => {
            let (host, path) = stored_sftp_transfer_endpoint(endpoint)?;
            (host, Some(path))
        }
        None => (None, None),
    };
    let WorkspaceSessionTab::SftpTransfer {
        machine_id,
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
        return Err(AppError::conflict("工作区标签不再是 SFTP 传输标签"));
    };
    if external_launch_id.is_some() {
        return Err(AppError::invalid("临时连接不能写入可恢复工作区"));
    }
    *machine_id = right_host
        .as_ref()
        .or(left_host.as_ref())
        .map(|host| host.as_str().to_owned())
        .unwrap_or_else(|| "local".to_owned());
    *left_host_id = left_host;
    *locked_left_host_id = None;
    *right_host_id = right_host;
    *initial_left_path = Some(left_path);
    *initial_right_path = right_path;
    *initial_right_selection = None;
    Ok(())
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
        Some(host_id) => Some(FileEndpoint::Remote {
            host_id: host_id.clone(),
            path: RemotePath::parse(initial_right_path.as_deref().unwrap_or("/"))
                .map_err(|_| WorkspaceFileRestoreIssue::InvalidPath)?,
        }),
        None => initial_right_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .map(|path| FileEndpoint::Local {
                path: Path::new(path).to_path_buf(),
            }),
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
    if runtime_tab.transient {
        let WorkspaceSessionTab::WorkspaceFile {
            target: registered_target,
            path: registered_path,
            source: registered_source,
            ..
        } = &runtime_tab.tab
        else {
            return None;
        };
        return (target == registered_target
            && path == registered_path
            && source == registered_source)
            .then(|| runtime_tab.endpoint.clone())
            .flatten();
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

#[cfg(test)]
#[path = "../../tests/unit/root_workspace_file_access.rs"]
mod file_access_tests;
