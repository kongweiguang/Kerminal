//! 文件树的惰性目录缓存与端点级选择模型。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui_kit::base::{TreeItem, TreeState};
use gpui_kit::{Context, Entity, Subscription};
use kerminal_domain::files::{DirectoryListing, FileEndpoint, FileEntry, FileEntryKind};
use tokio_util::sync::CancellationToken;

use super::selection::SelectionGesture;

/// 每个端点只缓存其直接子项；展开时才通过 FileService 读取下一层目录。
#[derive(Default)]
pub(super) struct FileTreeBrowser {
    root: Option<FileEndpoint>,
    root_generation: u64,
    revision: u64,
    next_request_id: u64,
    next_node_id: u64,
    directories: HashMap<FileEndpoint, TreeDirectoryState>,
    expanded: HashSet<FileEndpoint>,
    selected: HashSet<FileEndpoint>,
    anchor: Option<FileEndpoint>,
    focused: Option<FileEndpoint>,
    node_ids: HashMap<TreeNodeKey, String>,
    node_by_id: HashMap<String, TreeNode>,
}

#[derive(Clone)]
enum TreeDirectoryState {
    Unloaded,
    Loading {
        request_id: u64,
        cancellation: Arc<TreeRequestCancellation>,
    },
    Loaded(Vec<FileEntry>),
    Failed(String),
}

/// 将树行映射回完整 endpoint；重试占位行显式保存它所属的目录目标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TreeNode {
    Entry(FileEndpoint),
    Retry(FileEndpoint),
    Placeholder,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum TreeNodeKey {
    Entry(FileEndpoint),
    Placeholder {
        directory: FileEndpoint,
        kind: PlaceholderKind,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum PlaceholderKind {
    Unloaded,
    Loading,
    Empty,
    Failed,
}

/// 虚拟树按完整 endpoint 操作，不使用 basename 反查文件项。
#[derive(Clone, Default)]
pub(super) struct TreeNodeMap {
    pub(super) entries: HashMap<FileEndpoint, FileEntry>,
    pub(super) nodes: HashMap<String, TreeNode>,
    pub(super) visible_endpoints: Vec<FileEndpoint>,
    pub(super) selected: HashSet<FileEndpoint>,
}

/// 一次投影供 TreeState 更新和虚拟行绘制共用，防止列表顺序与范围选择分叉。
pub(super) struct TreeProjection {
    pub(super) items: Vec<TreeItem>,
    pub(super) nodes: TreeNodeMap,
    pub(super) focused_id: Option<String>,
    pub(super) revision: u64,
}

/// 树批量传输只保留未被已选目录覆盖的根项，并记录明确合并的选择数。
pub(super) struct TreeTransferSelectionPlan {
    pub(super) roots: Vec<FileEntry>,
    pub(super) included_descendants: usize,
}

/// 每侧独立持有 GPUI 虚拟树状态及订阅，避免左右面板共享焦点或展开事件。
pub(super) struct TreeWidget {
    pub(super) state: Entity<TreeState>,
    pub(super) node_map: TreeNodeMap,
    pub(super) projected_revision: u64,
    _event_subscription: Subscription,
    _observer_subscription: Subscription,
}

impl TreeWidget {
    /// 分别订阅用户展开/折叠事件与键盘焦点变更，让 GPUI Tree 只维护交互焦点。
    ///
    /// @author kongweiguang
    pub(super) fn new(
        side: usize,
        state: Entity<TreeState>,
        cx: &mut Context<super::FilesPane>,
    ) -> Self {
        let event_subscription = cx.subscribe(&state, move |pane, tree, event, cx| {
            pane.handle_tree_event(side, tree, event.clone(), cx);
        });
        let observer_subscription = cx.observe(&state, move |pane, tree, cx| {
            pane.handle_tree_focus_changed(side, tree, cx);
        });
        Self {
            state,
            node_map: TreeNodeMap::default(),
            projected_revision: u64::MAX,
            _event_subscription: event_subscription,
            _observer_subscription: observer_subscription,
        }
    }
}

/// 取消可在 executor future 启动前发生；先记失效标记，再绑定监督 token。
#[derive(Default)]
pub(super) struct TreeRequestCancellation {
    cancelled: AtomicBool,
    task_token: Mutex<Option<CancellationToken>>,
}

impl TreeRequestCancellation {
    /// 将监督执行器的 token 绑定到树读取；提前取消的请求会在绑定时立即停止。
    ///
    /// @author kongweiguang
    pub(super) fn bind_task_token(&self, token: CancellationToken) {
        let cancel_now = {
            let mut task_token = self
                .task_token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.cancelled.load(Ordering::Acquire) {
                true
            } else {
                *task_token = Some(token.clone());
                false
            }
        };
        if cancel_now {
            token.cancel();
        }
    }

    /// 取消当前子目录读取；只影响此读取，不关闭 SSH lease 或其它文件任务。
    ///
    /// @author kongweiguang
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let task_token = self
            .task_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(token) = task_token {
            token.cancel();
        }
    }
}

impl FileTreeBrowser {
    /// 根目录请求开始时取消旧代次的子目录读取，避免迟到结果写入新路径。
    ///
    /// @author kongweiguang
    pub(super) fn begin_root_request(&mut self, endpoint: &FileEndpoint, generation: u64) {
        self.cancel_pending_requests();
        if self.root.as_ref() == Some(endpoint) {
            self.directories
                .retain(|directory, _| directory == endpoint);
            self.expanded.clear();
            self.prune_selection_to_root();
        }
        self.root_generation = generation;
        self.touch();
    }

    /// 用已验证的根目录结果更新缓存；同根刷新保留仍可见的根项选择。
    /// 返回因根刷新而失效的选择数，由调用方给出可见反馈。
    ///
    /// @author kongweiguang
    pub(super) fn set_root_listing(
        &mut self,
        directory: FileEndpoint,
        generation: u64,
        entries: Vec<FileEntry>,
        show_hidden: bool,
    ) -> usize {
        if self.root_generation != generation {
            return 0;
        }

        let same_root = self.root.as_ref() == Some(&directory);
        let selected_before = self.selected.len();
        if !same_root {
            self.clear_cached_tree();
            self.root = Some(directory.clone());
        } else {
            self.directories
                .retain(|cached_directory, _| cached_directory == &directory);
            self.expanded.clear();
            let available = entries
                .iter()
                .map(|entry| entry.endpoint.clone())
                .collect::<HashSet<_>>();
            self.selected
                .retain(|endpoint| available.contains(endpoint));
            if self
                .anchor
                .as_ref()
                .is_some_and(|anchor| !available.contains(anchor))
            {
                self.anchor = None;
            }
            if self
                .focused
                .as_ref()
                .is_some_and(|focused| !available.contains(focused))
            {
                self.focused = None;
            }
        }

        self.root = Some(directory.clone());
        self.root_generation = generation;
        self.directories
            .insert(directory, TreeDirectoryState::Loaded(entries));
        self.prune_selection_to_loaded_visible(show_hidden);
        self.touch();
        selected_before.saturating_sub(self.selected.len())
    }

    /// 读取指定代次下的惰性目录；同一目录在加载或已缓存期间不会重复发请求。
    ///
    /// @author kongweiguang
    pub(super) fn begin_directory_request(
        &mut self,
        endpoint: &FileEndpoint,
        generation: u64,
    ) -> Option<(u64, Arc<TreeRequestCancellation>)> {
        if self.root_generation != generation || !self.is_directory_entry(endpoint) {
            return None;
        }
        match self.directories.get(endpoint) {
            Some(TreeDirectoryState::Loading { .. } | TreeDirectoryState::Loaded(_)) => {
                return None;
            }
            Some(TreeDirectoryState::Unloaded | TreeDirectoryState::Failed(_)) | None => {}
        }

        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        let request_id = self.next_request_id;
        let cancellation = Arc::new(TreeRequestCancellation::default());
        self.directories.insert(
            endpoint.clone(),
            TreeDirectoryState::Loading {
                request_id,
                cancellation: Arc::clone(&cancellation),
            },
        );
        self.touch();
        Some((request_id, cancellation))
    }

    /// 只接受仍属于当前树代次且 request id 匹配的结果，错误保留在目录行供显式重试。
    ///
    /// @author kongweiguang
    pub(super) fn complete_directory_request(
        &mut self,
        endpoint: &FileEndpoint,
        generation: u64,
        request_id: u64,
        show_hidden: bool,
        result: Result<DirectoryListing, String>,
    ) -> bool {
        if self.root_generation != generation
            || !matches!(
                self.directories.get(endpoint),
                Some(TreeDirectoryState::Loading {
                    request_id: current,
                    ..
                }) if *current == request_id
            )
        {
            return false;
        }

        let state = match result {
            Ok(listing)
                if listing.directory == *endpoint
                    && listing.entries.iter().all(|entry| {
                        super::parent_endpoint(&entry.endpoint).as_ref() == Some(endpoint)
                            && super::endpoint_name(&entry.endpoint).as_deref()
                                == Some(entry.name.as_str())
                    }) =>
            {
                TreeDirectoryState::Loaded(listing.entries)
            }
            Ok(_) => TreeDirectoryState::Failed(
                locale::text(
                    "服务返回了不同目录，点击重试",
                    "The file service returned a different directory. Click to retry.",
                )
                .to_string(),
            ),
            Err(message) => TreeDirectoryState::Failed(message),
        };
        self.directories.insert(endpoint.clone(), state);
        self.prune_selection_to_loaded_visible(show_hidden);
        self.touch();
        true
    }

    /// 记录 GPUI Tree 的真实折叠状态；折叠时清除后代选择，避免不可见项目被批处理。
    ///
    /// @author kongweiguang
    pub(super) fn collapse(&mut self, endpoint: &FileEndpoint) -> usize {
        self.expanded.remove(endpoint);
        self.expanded
            .retain(|candidate| !is_descendant_endpoint(candidate, endpoint));
        for (directory, state) in &mut self.directories {
            if (directory == endpoint || is_descendant_endpoint(directory, endpoint))
                && let TreeDirectoryState::Loading { cancellation, .. } = state
            {
                cancellation.cancel();
                *state = TreeDirectoryState::Unloaded;
            }
        }
        let before = self.selected.len();
        self.selected.retain(|candidate| {
            candidate == endpoint || !is_descendant_endpoint(candidate, endpoint)
        });
        if self
            .anchor
            .as_ref()
            .is_some_and(|anchor| is_descendant_endpoint(anchor, endpoint))
        {
            self.anchor = None;
        }
        if self
            .focused
            .as_ref()
            .is_some_and(|focused| is_descendant_endpoint(focused, endpoint))
        {
            self.focused = Some(endpoint.clone());
        }
        self.touch();
        before.saturating_sub(self.selected.len())
    }

    /// 保留展开标记，让已读取的子目录在折叠后再次展开时直接使用缓存。
    ///
    /// @author kongweiguang
    pub(super) fn expand(&mut self, endpoint: &FileEndpoint) {
        if self.expanded.insert(endpoint.clone()) {
            self.touch();
        }
    }

    /// 将一个完整 endpoint 设为树焦点，不改变 Ctrl/Shift 批量选择集合。
    ///
    /// @author kongweiguang
    pub(super) fn set_focused(&mut self, endpoint: Option<FileEndpoint>) {
        self.focused = endpoint;
    }

    /// 读取缓存中的实际 FileEntry；精确匹配 endpoint，不按重复 basename 降级。
    ///
    /// @author kongweiguang
    pub(super) fn entry(&self, endpoint: &FileEndpoint) -> Option<FileEntry> {
        self.directories.values().find_map(|state| match state {
            TreeDirectoryState::Loaded(entries) => entries
                .iter()
                .find(|entry| &entry.endpoint == endpoint)
                .cloned(),
            _ => None,
        })
    }

    /// 获取完整可见树的展开顺序，范围选择和批量操作共用这一份深度优先顺序。
    ///
    /// @author kongweiguang
    pub(super) fn visible_endpoints(&self, show_hidden: bool) -> Vec<FileEndpoint> {
        let Some(root) = self.root.as_ref() else {
            return Vec::new();
        };
        let Some(TreeDirectoryState::Loaded(entries)) = self.directories.get(root) else {
            return Vec::new();
        };
        let mut visible = Vec::new();
        let mut visited = HashSet::new();
        self.collect_visible(entries, show_hidden, &mut visible, &mut visited);
        visible
    }

    /// 按可见 endpoint 顺序返回实际选中条目，隐藏或已折叠后代不会进入批量动作。
    ///
    /// @author kongweiguang
    pub(super) fn selected_entries(&self, show_hidden: bool) -> Vec<FileEntry> {
        self.visible_endpoints(show_hidden)
            .into_iter()
            .filter(|endpoint| self.selected.contains(endpoint))
            .filter_map(|endpoint| self.entry(&endpoint))
            .collect()
    }

    /// 将树选择折叠成实际传输根项；完整 endpoint 和已加载的 DFS 选择顺序保持稳定。
    ///
    /// @author kongweiguang
    pub(super) fn transfer_selection_plan(&self, show_hidden: bool) -> TreeTransferSelectionPlan {
        let selected = self.selected_entries(show_hidden);
        let selected_count = selected.len();
        let selected_directories = selected
            .iter()
            .filter(|entry| entry.kind == FileEntryKind::Directory)
            .map(|entry| entry.endpoint.clone())
            .collect::<HashSet<_>>();
        let roots = selected
            .into_iter()
            .filter(|entry| {
                !has_selected_directory_ancestor(&entry.endpoint, &selected_directories)
            })
            .collect::<Vec<_>>();

        TreeTransferSelectionPlan {
            included_descendants: selected_count.saturating_sub(roots.len()),
            roots,
        }
    }

    /// 按树的 DFS 后序处理显式选择，使父项晚于已选后代且不扩大删除范围。
    ///
    /// @author kongweiguang
    pub(super) fn selected_entries_postorder(&self, show_hidden: bool) -> Vec<FileEntry> {
        selected_entries_in_postorder(self.selected_entries(show_hidden))
    }

    /// 获取当前根目录的可见项，用于 List/Tree 切换时只同步可见且可操作的选择。
    ///
    /// @author kongweiguang
    pub(super) fn root_entries(&self, show_hidden: bool) -> Vec<FileEntry> {
        let Some(root) = self.root.as_ref() else {
            return Vec::new();
        };
        let Some(TreeDirectoryState::Loaded(entries)) = self.directories.get(root) else {
            return Vec::new();
        };
        sorted_visible_entries(entries, show_hidden)
    }

    /// 查询完整 endpoint 是否处于批量选择中，树行复选状态不使用显示名推断。
    ///
    /// @author kongweiguang
    pub(super) fn is_selected(&self, endpoint: &FileEndpoint) -> bool {
        self.selected.contains(endpoint)
    }

    /// 应用列表一致的单选、Ctrl 切换、Shift 范围和 Ctrl+Shift 累加范围手势。
    ///
    /// @author kongweiguang
    pub(super) fn apply_selection(
        &mut self,
        endpoint: &FileEndpoint,
        visible: &[FileEndpoint],
        gesture: SelectionGesture,
    ) {
        if !visible.iter().any(|candidate| candidate == endpoint) {
            return;
        }
        match gesture {
            SelectionGesture::Replace => {
                self.selected.clear();
                self.selected.insert(endpoint.clone());
                self.anchor = Some(endpoint.clone());
            }
            SelectionGesture::Toggle => {
                if !self.selected.remove(endpoint) {
                    self.selected.insert(endpoint.clone());
                }
                self.anchor = Some(endpoint.clone());
            }
            SelectionGesture::Extend | SelectionGesture::AddRange => {
                let Some(target) = visible.iter().position(|candidate| candidate == endpoint)
                else {
                    return;
                };
                let anchor = self
                    .anchor
                    .as_ref()
                    .and_then(|anchor| visible.iter().position(|candidate| candidate == anchor))
                    .unwrap_or(target);
                if gesture == SelectionGesture::Extend {
                    self.selected.clear();
                }
                for candidate in &visible[anchor.min(target)..=anchor.max(target)] {
                    self.selected.insert(candidate.clone());
                }
                self.anchor = Some(visible[anchor].clone());
            }
        }
        self.touch();
    }

    /// 全选或清除当前真实可见节点；未加载或折叠子树不被悄悄纳入操作。
    ///
    /// @author kongweiguang
    pub(super) fn toggle_select_all(&mut self, visible: &[FileEndpoint]) {
        if !visible.is_empty()
            && visible
                .iter()
                .all(|endpoint| self.selected.contains(endpoint))
        {
            self.selected.clear();
            self.anchor = None;
            self.touch();
            return;
        }
        self.selected = visible.iter().cloned().collect();
        self.anchor = visible.first().cloned();
        self.touch();
    }

    /// 只选择一个真实节点，供打开编辑器、键盘焦点和从列表切换模式时同步使用。
    ///
    /// @author kongweiguang
    pub(super) fn select_only(&mut self, endpoint: &FileEndpoint) {
        self.selected.clear();
        self.selected.insert(endpoint.clone());
        self.anchor = Some(endpoint.clone());
        self.focused = Some(endpoint.clone());
        self.touch();
    }

    /// 同步来自一层列表的完整 endpoints；不依赖显示名，因此同名目录不会合并。
    ///
    /// @author kongweiguang
    pub(super) fn select_endpoints(&mut self, endpoints: impl IntoIterator<Item = FileEndpoint>) {
        let endpoints = endpoints.into_iter().collect::<Vec<_>>();
        self.anchor = endpoints.first().cloned();
        self.selected = endpoints.into_iter().collect();
        if self
            .focused
            .as_ref()
            .is_none_or(|focused| !self.selected.contains(focused))
        {
            self.focused = self.anchor.clone();
        }
        self.touch();
    }

    /// 过滤切换或刷新后清除不再可见的选择，并返回清除数量供 UI 明确提示。
    ///
    /// @author kongweiguang
    pub(super) fn retain_visible_selection(&mut self, show_hidden: bool) -> usize {
        let visible = self
            .visible_endpoints(show_hidden)
            .into_iter()
            .collect::<HashSet<_>>();
        let before = self.selected.len();
        self.selected.retain(|endpoint| visible.contains(endpoint));
        if self
            .anchor
            .as_ref()
            .is_some_and(|anchor| !visible.contains(anchor))
        {
            self.anchor = None;
        }
        self.touch();
        before.saturating_sub(self.selected.len())
    }

    /// 按完整 endpoint 生成 GPUI TreeItem；所有目录都带有可展开占位行以触发惰性读取。
    ///
    /// @author kongweiguang
    pub(super) fn projection(&mut self, show_hidden: bool) -> TreeProjection {
        let root_entries = self
            .root
            .as_ref()
            .and_then(|root| self.directories.get(root))
            .and_then(|state| match state {
                TreeDirectoryState::Loaded(entries) => Some(entries.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let mut nodes = TreeNodeMap::default();
        let mut items = Vec::new();
        for entry in sorted_visible_entries(&root_entries, show_hidden) {
            items.push(self.build_entry_item(entry, show_hidden, &mut nodes));
        }
        nodes.visible_endpoints = self.visible_endpoints(show_hidden);
        let visible = nodes
            .visible_endpoints
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        nodes.selected = self.selected.intersection(&visible).cloned().collect();
        let focused_id = self
            .focused
            .as_ref()
            .and_then(|focused| self.node_ids.get(&TreeNodeKey::Entry(focused.clone())))
            .cloned();
        self.node_by_id = nodes.nodes.clone();
        TreeProjection {
            items,
            nodes,
            focused_id,
            revision: self.revision,
        }
    }

    /// 取消所有仍在途的树子目录读取；缓存结果保留，下一次展开可以明确重试。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_pending_requests(&mut self) {
        let mut changed = false;
        for state in self.directories.values_mut() {
            if let TreeDirectoryState::Loading { cancellation, .. } = state {
                cancellation.cancel();
                *state = TreeDirectoryState::Unloaded;
                changed = true;
            }
        }
        if changed {
            self.touch();
        }
    }

    /// 将树目录加载状态映射为用户可读占位行；失败行绑定父目录 endpoint 供显式重试。
    ///
    /// @author kongweiguang
    fn build_entry_item(
        &mut self,
        entry: FileEntry,
        show_hidden: bool,
        nodes: &mut TreeNodeMap,
    ) -> TreeItem {
        let endpoint = entry.endpoint.clone();
        let node_id = self.node_id(TreeNodeKey::Entry(endpoint.clone()));
        self.node_by_id
            .insert(node_id.clone(), TreeNode::Entry(endpoint.clone()));
        nodes
            .nodes
            .insert(node_id.clone(), TreeNode::Entry(endpoint.clone()));
        nodes.entries.insert(endpoint.clone(), entry.clone());

        let children = if entry.kind == FileEntryKind::Directory {
            let state = self
                .directories
                .get(&endpoint)
                .cloned()
                .unwrap_or(TreeDirectoryState::Unloaded);
            match state {
                TreeDirectoryState::Loaded(entries) => {
                    let visible = sorted_visible_entries(&entries, show_hidden);
                    if visible.is_empty() {
                        vec![self.placeholder_item(
                            &endpoint,
                            PlaceholderKind::Empty,
                            locale::text("空目录", "Empty directory").to_string(),
                            true,
                            nodes,
                        )]
                    } else {
                        visible
                            .into_iter()
                            .map(|entry| self.build_entry_item(entry, show_hidden, nodes))
                            .collect()
                    }
                }
                TreeDirectoryState::Loading { .. } => vec![self.placeholder_item(
                    &endpoint,
                    PlaceholderKind::Loading,
                    locale::text("正在读取…", "Loading…").to_string(),
                    true,
                    nodes,
                )],
                TreeDirectoryState::Failed(message) => vec![
                    self.placeholder_item(
                        &endpoint,
                        PlaceholderKind::Failed,
                        locale::text(
                            format!("加载失败 · {message} · 点击重试"),
                            format!("Load failed · {message} · click to retry"),
                        )
                        .to_string(),
                        false,
                        nodes,
                    ),
                ],
                TreeDirectoryState::Unloaded => vec![self.placeholder_item(
                    &endpoint,
                    PlaceholderKind::Unloaded,
                    locale::text("展开以读取目录", "Expand to read directory").to_string(),
                    true,
                    nodes,
                )],
            }
        } else {
            Vec::new()
        };

        let accessible_label = if self.selected.contains(&endpoint) {
            locale::text(
                format!("{}，已选择", entry.name),
                format!("{}, selected", entry.name),
            )
            .to_string()
        } else {
            entry.name
        };
        TreeItem::new(node_id, accessible_label)
            .children(children)
            .expanded(self.expanded.contains(&endpoint))
    }

    /// 占位节点使用与文件不同的身份键，避免同一路径状态行与真实文件名相撞。
    ///
    /// @author kongweiguang
    fn placeholder_item(
        &mut self,
        directory: &FileEndpoint,
        kind: PlaceholderKind,
        label: String,
        disabled: bool,
        nodes: &mut TreeNodeMap,
    ) -> TreeItem {
        let id = self.node_id(TreeNodeKey::Placeholder {
            directory: directory.clone(),
            kind,
        });
        nodes.nodes.insert(
            id.clone(),
            if kind == PlaceholderKind::Failed {
                TreeNode::Retry(directory.clone())
            } else {
                TreeNode::Placeholder
            },
        );
        TreeItem::new(id, label).disabled(disabled)
    }

    /// 为当前投影中的不同节点分配稳定且不暴露路径的 UI id。
    ///
    /// @author kongweiguang
    fn node_id(&mut self, key: TreeNodeKey) -> String {
        if let Some(id) = self.node_ids.get(&key) {
            return id.clone();
        }
        self.next_node_id = self.next_node_id.wrapping_add(1).max(1);
        let id = format!("file-tree-node-{}", self.next_node_id);
        self.node_ids.insert(key, id.clone());
        id
    }

    /// 收集可见深度优先端点顺序；endpoint 去重可阻止异常 provider 响应造成递归循环。
    ///
    /// @author kongweiguang
    fn collect_visible(
        &self,
        entries: &[FileEntry],
        show_hidden: bool,
        output: &mut Vec<FileEndpoint>,
        visited: &mut HashSet<FileEndpoint>,
    ) {
        for entry in sorted_visible_entries(entries, show_hidden) {
            if !visited.insert(entry.endpoint.clone()) {
                continue;
            }
            output.push(entry.endpoint.clone());
            if entry.kind != FileEntryKind::Directory || !self.expanded.contains(&entry.endpoint) {
                continue;
            }
            if let Some(TreeDirectoryState::Loaded(children)) =
                self.directories.get(&entry.endpoint)
            {
                self.collect_visible(children, show_hidden, output, visited);
            }
        }
    }

    /// 目录读取成功后只允许普通目录节点触发后续请求，符号链接始终保留叶节点语义。
    ///
    /// @author kongweiguang
    fn is_directory_entry(&self, endpoint: &FileEndpoint) -> bool {
        self.directories.values().any(|state| {
            matches!(state, TreeDirectoryState::Loaded(entries) if entries.iter().any(|entry| {
                &entry.endpoint == endpoint && entry.kind == FileEntryKind::Directory
            }))
        })
    }

    /// 接收目录响应后删除不再可见选择，批量操作不会落到折叠或隐藏的旧节点。
    ///
    /// @author kongweiguang
    fn prune_selection_to_loaded_visible(&mut self, show_hidden: bool) {
        let visible = self
            .visible_endpoints(show_hidden)
            .into_iter()
            .collect::<HashSet<_>>();
        self.selected.retain(|endpoint| visible.contains(endpoint));
        if self
            .anchor
            .as_ref()
            .is_some_and(|anchor| !visible.contains(anchor))
        {
            self.anchor = None;
        }
        if self
            .focused
            .as_ref()
            .is_some_and(|focused| !visible.contains(focused))
        {
            self.focused = None;
        }
    }

    /// 清空旧根的缓存、节点 id 和交互状态；不同 endpoint 不共享目录缓存。
    ///
    /// @author kongweiguang
    fn clear_cached_tree(&mut self) {
        self.cancel_pending_requests();
        self.root = None;
        self.directories.clear();
        self.expanded.clear();
        self.selected.clear();
        self.anchor = None;
        self.focused = None;
        self.node_ids.clear();
        self.node_by_id.clear();
    }

    /// 递增结构代次，令 GPUI TreeState 在下一次渲染时只应用最新投影。
    ///
    /// @author kongweiguang
    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// 根目录刷新时只保留第一层有效选择，避免旧子树节点隐藏在加载状态中。
    ///
    /// @author kongweiguang
    fn prune_selection_to_root(&mut self) {
        let visible = self
            .root_entries(true)
            .into_iter()
            .map(|entry| entry.endpoint)
            .collect::<HashSet<_>>();
        self.selected.retain(|endpoint| visible.contains(endpoint));
        if self
            .anchor
            .as_ref()
            .is_some_and(|anchor| !visible.contains(anchor))
        {
            self.anchor = None;
        }
        if self
            .focused
            .as_ref()
            .is_some_and(|focused| !visible.contains(focused))
        {
            self.focused = None;
        }
    }
}

/// 使用文件列表相同的目录优先排序规则，保证树与 List 模式的可见顺序一致。
///
/// @author kongweiguang
fn sorted_visible_entries(entries: &[FileEntry], show_hidden: bool) -> Vec<FileEntry> {
    let mut visible = entries
        .iter()
        .filter(|entry| show_hidden || !super::render::browser::is_hidden_entry(entry))
        .cloned()
        .collect::<Vec<_>>();
    visible.sort_by(|left, right| {
        super::render::browser::entry_sort_key(left)
            .cmp(&super::render::browser::entry_sort_key(right))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    visible
}

/// 按 endpoint 的真实父关系判断目录后代，不对 POSIX 路径做字符串前缀判断。
///
/// @author kongweiguang
pub(super) fn is_descendant_endpoint(candidate: &FileEndpoint, ancestor: &FileEndpoint) -> bool {
    let mut current = candidate.clone();
    while let Some(parent) = super::parent_endpoint(&current) {
        if &parent == ancestor {
            return true;
        }
        if parent == current {
            return false;
        }
        current = parent;
    }
    false
}

/// 沿完整 endpoint 的父链查找已选目录，避免同名路径或相似前缀被误合并。
///
/// @author kongweiguang
fn has_selected_directory_ancestor(
    candidate: &FileEndpoint,
    selected_directories: &HashSet<FileEndpoint>,
) -> bool {
    let mut current = candidate.clone();
    while let Some(parent) = super::parent_endpoint(&current) {
        if parent == current {
            return false;
        }
        if selected_directories.contains(&parent) {
            return true;
        }
        current = parent;
    }
    false
}

/// 将 Tree 已提供的 preorder 变为稳定后序；栈只保留当前父目录链，避免全量两两比较。
///
/// @author kongweiguang
fn selected_entries_in_postorder(entries: Vec<FileEntry>) -> Vec<FileEntry> {
    let mut ordered = Vec::with_capacity(entries.len());
    let mut directories = Vec::<FileEntry>::new();

    for entry in entries {
        while directories
            .last()
            .is_some_and(|directory| !is_descendant_endpoint(&entry.endpoint, &directory.endpoint))
        {
            if let Some(directory) = directories.pop() {
                ordered.push(directory);
            }
        }

        if entry.kind == FileEntryKind::Directory {
            directories.push(entry);
        } else {
            ordered.push(entry);
        }
    }

    ordered.extend(directories.into_iter().rev());
    ordered
}

use crate::locale;

#[cfg(test)]
#[path = "../../../tests/unit/files_tree.rs"]
mod tests;
