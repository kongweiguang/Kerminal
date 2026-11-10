//! 文件上下文的显式宿主边界，避免文件激活被误解释为终端目录跟随。
//!
//! @author kongweiguang

use gpui_kit::{App, Context, Pixels, Point, base::ScrollbarHandle as _};
use kerminal_application::{
    AppError, AppResult,
    platform::external_launch::ExternalTargetLease,
    sessions::UiSessionBinding,
    transfers::{TransferOrigin, TransferQueryScope},
};
use kerminal_domain::{
    files::{DirectoryListing, FileEndpoint, FileEntry},
    identity::TabId,
};

use super::{
    BrowserPanel, FileListKeyboardFocus, FileSort, FilesPane, NavigationHistory,
    endpoint_same_filesystem, localized_error,
    selection::FileSelection,
    tree::{FileTreeBrowser, FileTreeContextSnapshot},
    view_mode::{FileEntryFilter, FilesViewMode},
};

/// 缺少终端 binding 不能代表文件来源；容器也始终使用它自己的文件系统身份。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CompactFilesContext {
    Terminal(UiSessionBinding),
    FileWorkspace(TabId),
    Unavailable,
}

/// 跨顶层标签只交接已提交的浏览值，不共享 Entity、滚动控制器或任何请求/写入所有权。
#[derive(Clone, Debug)]
pub(crate) struct FileBrowserContextSnapshot {
    endpoint: FileEndpoint,
    listing: DirectoryListing,
    selected: Option<FileEntry>,
    selection: FileSelection,
    keyboard_focus: FileListKeyboardFocus,
    navigation_history: NavigationHistory,
    sort: FileSort,
    entry_filter: FileEntryFilter,
    show_hidden_files: bool,
    scroll_offset: Point<Pixels>,
    tree_context: Option<FileTreeContextSnapshot>,
    tree_scroll_offset: Point<Pixels>,
    /// 只保留已观察提交的纯值基线，避免新实体把旧成功任务误判成新目录变更。
    observed_transfer_rows: Vec<kerminal_domain::files::TransferSummary>,
}

/// 目录打开是显式用户意图，Root 负责工作区去重和内部标签 owner。
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct OpenDirectoryInWorkspaceRequested {
    pub endpoint: FileEndpoint,
    pub lease: Option<ExternalTargetLease>,
}

/// 只在目录读取成功提交后投影路径，失败或迟到请求不能污染持久描述。
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct FileWorkspaceDirectoryChanged {
    pub endpoint: FileEndpoint,
}

impl FilesPane {
    /// 只交接已经提交且当前仍可操作的列表；在途导航、树读取和写入不能被伪装成完成快照。
    /// 快照不携带终端能力、租约或取消 owner，源面板仍独立拥有全部异步生命周期。
    ///
    /// @author kongweiguang
    pub(crate) fn capture_file_browser_context(
        &self,
        cx: &App,
    ) -> Option<FileBrowserContextSnapshot> {
        let panel = &self.panels[1];
        if !self.compact_layout
            || !self.compact_target_is_available()
            || panel.loading
            || panel.requested_endpoint.is_some()
            || panel.directory_error.is_some()
            || panel.failed_directory.is_some()
            || self.file_workspace_directory_close_blocked()
        {
            return None;
        }
        let listing = panel
            .listing
            .as_ref()
            .filter(|listing| listing.directory == panel.endpoint)?;
        let tree_context = if self.view_mode == FilesViewMode::Tree {
            Some(panel.tree.capture_context()?)
        } else {
            None
        };
        let tree_scroll_offset = panel
            .tree_widget
            .as_ref()
            .map(|widget| widget.state.read(cx).scroll_handle().offset())
            .unwrap_or_default();
        Some(FileBrowserContextSnapshot {
            endpoint: panel.endpoint.clone(),
            listing: listing.clone(),
            selected: panel.selected.clone(),
            selection: panel.selection.clone(),
            keyboard_focus: panel.keyboard_focus.clone(),
            navigation_history: panel.navigation_history.clone(),
            sort: panel.sort,
            entry_filter: panel.entry_filter,
            show_hidden_files: self.show_hidden_files,
            scroll_offset: panel.list_scroll.offset(),
            tree_context,
            tree_scroll_offset,
            observed_transfer_rows: self.all_transfer_rows.clone(),
        })
    }

    /// 只恢复到未绑定的空闲右栏，并再次核对清单、选择和文件系统身份。
    /// 重新创建BrowserPanel和滚动控制器，不共享Entity；精确目标及lease仍由下一步setter授权。
    ///
    /// @author kongweiguang
    pub(crate) fn restore_file_browser_context(
        &mut self,
        snapshot: FileBrowserContextSnapshot,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.compact_layout
            || self.current_terminal_endpoint.is_some()
            || self.current_terminal_binding.is_some()
            || self.external_target_lease.is_some()
            || self.panels[1].loading
            || self.panels[1].listing.is_some()
            || self.panels[1].requested_endpoint.is_some()
            || self.panels[1].root_request_cancellation.is_some()
            || self.panels[1].directory_error.is_some()
            || self.panels[1].failed_directory.is_some()
            || self.file_workspace_directory_close_blocked()
            || snapshot.listing.directory != snapshot.endpoint
            || snapshot.navigation_history.current != snapshot.endpoint
            || snapshot
                .listing
                .entries
                .iter()
                .any(|entry| !endpoint_same_filesystem(&entry.endpoint, &snapshot.endpoint))
            || snapshot
                .tree_context
                .as_ref()
                .is_some_and(|tree| !tree.is_for_directory(&snapshot.endpoint))
            || snapshot.selected.as_ref().is_some_and(|selected| {
                !snapshot.listing.entries.contains(selected)
                    && !snapshot
                        .tree_context
                        .as_ref()
                        .is_some_and(|tree| tree.contains_entry(selected))
            })
        {
            return false;
        }
        let names = snapshot
            .listing
            .entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<Vec<_>>();
        let mut selection = snapshot.selection.clone();
        selection.retain_visible(&names);
        if selection != snapshot.selection {
            return false;
        }
        let tree_mode = snapshot.tree_context.is_some();
        let mut panel = BrowserPanel::new(snapshot.endpoint.clone());
        if let Some(tree_context) = snapshot.tree_context {
            panel.tree = FileTreeBrowser::from_context(tree_context, panel.generation);
        }
        panel.listing = Some(snapshot.listing);
        panel.selected = snapshot.selected;
        panel.selection = snapshot.selection;
        panel.keyboard_focus = snapshot.keyboard_focus;
        panel.navigation_history = snapshot.navigation_history;
        panel.sort = snapshot.sort;
        panel.entry_filter = snapshot.entry_filter;
        panel.list_scroll.set_offset(snapshot.scroll_offset);
        self.panels[1] = panel;
        self.active_side = 1;
        self.show_hidden_files = snapshot.show_hidden_files;
        self.view_mode = if tree_mode {
            FilesViewMode::Tree
        } else {
            FilesViewMode::List
        };
        if tree_mode {
            self.ensure_tree_widget(1, cx);
            if let Some(widget) = self.panels[1].tree_widget.as_ref() {
                widget
                    .state
                    .read(cx)
                    .scroll_handle()
                    .set_offset(snapshot.tree_scroll_offset);
            }
        }
        self.all_transfer_rows = snapshot.observed_transfer_rows;
        self.initialized = true;
        self.restored_file_browser_endpoint = Some(snapshot.endpoint);
        cx.notify();
        true
    }

    /// 文件激活只更新宿主身份；同文件系统保留浏览、选择、滚动和正在进行的目录请求。
    /// 首次绑定或跨文件系统才初始化，避免编辑器内部切文件反复读取父目录。
    ///
    /// @author kongweiguang
    pub(crate) fn set_file_workspace_target(
        &mut self,
        workspace_id: TabId,
        endpoint: FileEndpoint,
        lease: Option<ExternalTargetLease>,
        cx: &mut Context<Self>,
    ) {
        let retained = match self.retain_file_target_lease(&endpoint, lease.as_ref()) {
            Ok(retained) => retained,
            Err(error) => {
                let message = localized_error(&error);
                if !self.compact_target_error_is_current(&message) {
                    self.compact_context = CompactFilesContext::Unavailable;
                    self.current_terminal_endpoint = None;
                    self.current_terminal_binding = None;
                    self.external_target_lease = None;
                    self.invalidate_compact_target(cx);
                    self.status = Some(message);
                    cx.notify();
                }
                return;
            }
        };
        let restored = self.restored_file_browser_endpoint.take();
        let preserve = (self.compact_target_is_available()
            || restored.as_ref() == Some(&self.panels[1].endpoint))
            && endpoint_same_filesystem(&self.panels[1].endpoint, &endpoint);
        let context = CompactFilesContext::FileWorkspace(workspace_id);
        let changed = self.compact_context != context || self.external_target_lease != retained;
        self.compact_context = context;
        self.current_terminal_binding = None;
        self.current_terminal_endpoint = Some(endpoint.clone());
        self.external_target_lease = retained;
        self.terminal_directory_follow_generation = None;
        if !preserve {
            self.invalidate_compact_target(cx);
            self.panels[1].endpoint = endpoint.clone();
            self.panels[1].navigation_history.reset(endpoint.clone());
            self.open_endpoint_on_side(1, endpoint, cx);
        }
        if changed || !preserve {
            cx.notify();
        }
    }

    /// 租约只可用于其精确主机；同目标复用已保留的独立 scope，不随重绘重复申请。
    /// 容器仍保留 Container endpoint，绝不改写为宿主根目录。
    ///
    /// @author kongweiguang
    fn retain_file_target_lease(
        &self,
        endpoint: &FileEndpoint,
        lease: Option<&ExternalTargetLease>,
    ) -> AppResult<Option<ExternalTargetLease>> {
        if let Some(lease) = lease {
            if endpoint.host_id() != Some(lease.host_id()) {
                return Err(AppError::invalid("文件目标与连接租约不匹配"));
            }
            if let Some(current) = self.external_target_lease.as_ref()
                && current.host_id() == lease.host_id()
            {
                return Ok(Some(current.clone()));
            }
            return lease.retain().map(Some);
        }
        Ok(self
            .external_target_lease
            .as_ref()
            .filter(|current| endpoint.host_id() == Some(current.host_id()))
            .cloned())
    }

    /// 宿主目录页复用同一 FilesPane 实体，但只允许单栏文件布局；重复入口保持幂等。
    /// 这是文件工作区与双端 SFTP 工作台的边界，不能靠调用方偶然保持。
    ///
    /// @author kongweiguang
    pub(crate) fn ensure_host_workspace_layout(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        if !self.file_workspace_layout {
            self.file_workspace_layout = true;
            changed = true;
        }
        if self.compact_layout {
            self.compact_layout = false;
            changed = true;
        }
        if self.active_side != 1 {
            self.active_side = 1;
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// 每个内部目录由 Root 持有独立 Entity；此配置只去掉双端编排外壳，不另建浏览状态。
    /// 来源首项是新任务 owner，其余仅作历史别名，空集合拒绝以防任务无来源。
    ///
    /// @author kongweiguang
    pub(crate) fn configure_file_workspace(
        &mut self,
        endpoint: FileEndpoint,
        lease: Option<ExternalTargetLease>,
        origins: Vec<TransferOrigin>,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        let origin = origins
            .first()
            .cloned()
            .ok_or_else(|| AppError::invalid("文件工作区需要明确的传输来源"))?;
        let retained = self.retain_file_target_lease(&endpoint, lease.as_ref())?;
        let rebind = !self.file_workspace_layout
            || !endpoint_same_filesystem(&self.panels[1].endpoint, &endpoint);
        self.ensure_host_workspace_layout(cx);
        self.host_picker.enabled = false;
        self.active_side = 1;
        self.external_target_lease = retained;
        self.set_transfer_origin(Some(origin), cx);
        self.set_transfer_query_scope(TransferQueryScope::origins(origins), cx);
        if rebind {
            self.invalidate_compact_target(cx);
            self.panels[1].endpoint = endpoint.clone();
            self.panels[1].navigation_history.reset(endpoint.clone());
            if self.initialized {
                self.open_endpoint_on_side(1, endpoint, cx);
            }
        }
        cx.notify();
        Ok(())
    }

    /// 目录实体是已提交写操作与对话框的回执 owner，不能用移除实体冒充取消成功。
    /// 单纯读取可由 Drop 取消；写入、受理中传输、恢复归档和未决确认必须先完成或显式撤销。
    ///
    /// @author kongweiguang
    pub(crate) fn file_workspace_directory_close_blocked(&self) -> bool {
        self.transfer_enqueue_in_progress()
            || self.name_action_busy
            || self.permissions.busy
            || self.delete_action_busy
            || self.name_action.is_some()
            || self.permission_dialog_target.is_some()
            || self.delete_confirmation.is_some()
            || self.pending_transfer_conflict.is_some()
            || self.pending_host_key_trust.is_some()
            || self.queue.has_pending_retry()
            || self.queue.has_pending_recovery_dismissal()
            || self.queue.is_clearing_history()
    }

    /// Root 可在保存或订阅时读取已提交路径；不把在途导航冒充目录成功。
    ///
    /// @author kongweiguang
    pub(crate) fn current_directory_endpoint(&self) -> &FileEndpoint {
        &self.panels[1].endpoint
    }

    /// 显式定位才允许文件工作区采用当前文档父目录，普通内容激活不走此入口。
    ///
    /// @author kongweiguang
    pub(crate) fn locate_file_workspace_directory(
        &mut self,
        endpoint: FileEndpoint,
        cx: &mut Context<Self>,
    ) {
        if matches!(self.compact_context, CompactFilesContext::FileWorkspace(_))
            && self.compact_target_is_available()
            && endpoint_same_filesystem(&self.panels[1].endpoint, &endpoint)
            && self.panels[1]
                .requested_endpoint
                .as_ref()
                .unwrap_or(&self.panels[1].endpoint)
                != &endpoint
        {
            self.open_endpoint_on_side(1, endpoint, cx);
        }
    }

    /// 菜单冻结明确目录和独立连接 scope；Root 接受前切换来源不会把请求改投其它主机。
    ///
    /// @author kongweiguang
    pub(super) fn request_open_directory_in_workspace(
        &mut self,
        endpoint: FileEndpoint,
        cx: &mut Context<Self>,
    ) {
        let lease = match self
            .external_target_lease_for_endpoint(&endpoint)
            .map(|lease| lease.retain())
            .transpose()
        {
            Ok(lease) => lease,
            Err(error) => {
                self.status = Some(localized_error(&error));
                cx.notify();
                return;
            }
        };
        cx.emit(OpenDirectoryInWorkspaceRequested { endpoint, lease });
    }

    /// 查询切换只失效队列投影代次，不导航目录、不撤销入队准备，也不改创建来源。
    /// 精确来源集合同时作为“当前来源”的别名集合；全部视图不覆盖该集合。
    ///
    /// @author kongweiguang
    pub(crate) fn set_transfer_query_scope(
        &mut self,
        scope: TransferQueryScope,
        cx: &mut Context<Self>,
    ) {
        let scope = match scope {
            TransferQueryScope::Origins(origins) => TransferQueryScope::origins(origins),
            TransferQueryScope::All => TransferQueryScope::All,
        };
        if matches!(scope, TransferQueryScope::Origins(_)) {
            self.current_source_scope = scope.clone();
        }
        if self.transfer_query_scope == scope {
            return;
        }
        self.transfer_query_scope = scope;
        self.transfer_query_generation = self.transfer_query_generation.wrapping_add(1);
        self.transfer_rows.clear();
        self.transfer_recovery_rows.clear();
        let generation = self.queue.begin_transfer_refresh();
        self.queue.finish_recovery_refresh(generation, Ok(()));
        if self.initialized {
            self.reload_transfers(cx);
        }
        cx.notify();
    }

    /// 只有共享传输台提供全局显示开关，独立传输与右栏始终沿用调用方指定范围。
    ///
    /// @author kongweiguang
    pub(super) fn has_transfer_scope_selector(&self) -> bool {
        !self.compact_layout
            && !self.file_workspace_layout
            && self.transfer_origin == Some(TransferOrigin::SharedWorkbench)
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/files_workspace_context.rs"]
mod tests;
