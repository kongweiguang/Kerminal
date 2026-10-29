//! 工作区 session v3 的强类型 codec；更新时只改已知工作区字段并保留原始扩展。
//!
//! @author kongweiguang

use std::collections::{BTreeMap, BTreeSet};

use kerminal_domain::{
    identity::{HostId, PaneId, TabId},
    workspace::{SplitDirection, TerminalLayoutNode},
};
use serde_json::{Map, Value, json};

use super::WorkspaceSessionInvalidReason;

/// 标签组 ID 在冻结的 v3 中是对象键；应用层 newtype 防止误与 tab/pane 身份互换。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceTabGroupId(String);

impl WorkspaceTabGroupId {
    /// 读取 v3 组键时只拒绝空值和控制字符，保留既有非 UUID 标识。
    pub fn parse(value: &str) -> Result<Self, WorkspaceSessionInvalidReason> {
        if value.is_empty() || value.chars().any(char::is_control) {
            return Err(WorkspaceSessionInvalidReason::InvalidSchema);
        }
        Ok(Self(value.to_owned()))
    }

    /// 返回 JSON 对象键原文，调用方不得将其解释为文件路径。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 冻结 v3 的显式标签组；颜色保留未知字符串，未来主题值不会被写成默认色。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceTabGroup {
    pub title: String,
    pub color: Option<WorkspaceTabGroupColor>,
    pub collapsed: bool,
}

/// 颜色集合对应 v3 已知选项；Unsupported 用于无损往返未来颜色值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceTabGroupColor {
    Blue,
    Pink,
    Purple,
    Mint,
    Amber,
    Teal,
    Orange,
    Gray,
    Unsupported(String),
}

/// 三种冻结 v3 目标形状；Unavailable 保留原 JSON 并阻止 Root 猜测替代目标。
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceTargetRef {
    Local {
        profile_id: Option<String>,
    },
    Ssh {
        host_id: HostId,
    },
    Telnet {
        host_id: HostId,
    },
    Serial {
        host_id: HostId,
    },
    DockerContainer {
        host_id: HostId,
        container_id: String,
        runtime: WorkspaceContainerRuntime,
        container_name: Option<String>,
        user: Option<String>,
        workdir: Option<String>,
    },
    Unavailable {
        raw: Value,
        reason: WorkspaceTargetUnavailableReason,
    },
}

/// 容器运行时沿用 v3 的 docker/podman 字符串，不在恢复时探测并替换运行时。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceContainerRuntime {
    Docker,
    Podman,
}

/// 目标无法安全启动时给 UI 和 Root 的固定原因，不泄露配置正文。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceTargetUnavailableReason {
    Malformed,
    UnknownKind,
    InvalidHostId,
    MissingContainerId,
    UnknownRuntime,
}

/// 旧终端 pane 的持久化模式；preview/container 仍是 pane mode，不等同于 target kind。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspacePaneMode {
    Local,
    Ssh,
    Telnet,
    Serial,
    Container,
    Preview,
}

/// 只保留 v3 持久化的连接状态；恢复状态由 Root 的真实资源结果重新投影。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceMachineStatus {
    Online,
    Offline,
    Warning,
}

/// 本地终端来源仍区分侧栏 profile 与工作区默认 shell。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceLocalMachineScope {
    Sidebar,
    Workspace,
}

/// v3 pane 的连接和启动字段；SessionId 不属于持久化快照。
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceSessionPane {
    pub id: PaneId,
    pub title: String,
    pub machine_id: String,
    pub mode: WorkspacePaneMode,
    pub target: Option<WorkspaceTargetRef>,
    pub remote_host_id: Option<HostId>,
    pub container_id: Option<String>,
    pub profile_id: Option<String>,
    pub shell: Option<String>,
    pub args: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub current_cwd: Option<String>,
    pub env: Option<BTreeMap<String, String>>,
    pub remote_command: Option<String>,
    pub tmux_binding: Option<WorkspaceTmuxBinding>,
    pub prompt: String,
    pub status: WorkspaceMachineStatus,
    pub latency_ms: Option<f64>,
    pub local_machine_scope: Option<WorkspaceLocalMachineScope>,
    pub output_history: Option<String>,
}

/// tmux 绑定是持久化 pane 元数据；该快照不拥有正在运行的 tmux 连接。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceTmuxBinding {
    pub target_ref: String,
    pub session_id: String,
    pub session_name: String,
    pub attached_at: String,
    pub socket_name: Option<String>,
    pub socket_path: Option<String>,
}

/// SFTP transfer 和 workspace file 同样属于有序工作区 tab，但不伪造终端 layout。
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceSessionTab {
    Terminal {
        id: TabId,
        tab_group_id: Option<WorkspaceTabGroupId>,
        title: String,
        machine_id: String,
        layout: TerminalLayoutNode,
    },
    SftpTransfer {
        id: TabId,
        tab_group_id: Option<WorkspaceTabGroupId>,
        title: String,
        machine_id: String,
        left_host_id: Option<HostId>,
        locked_left_host_id: Option<HostId>,
        right_host_id: Option<HostId>,
        external_launch_id: Option<String>,
        /// 左端点起始路径；缺省时继续使用工作区原有的默认目录。
        initial_left_path: Option<String>,
        initial_right_path: Option<String>,
        initial_right_selection: Option<String>,
    },
    WorkspaceFile {
        id: TabId,
        tab_group_id: Option<WorkspaceTabGroupId>,
        title: String,
        machine_id: String,
        target: WorkspaceTargetRef,
        path: String,
        root_path: Option<String>,
        access: WorkspaceFileAccess,
        source: WorkspaceFileSource,
    },
    Unsupported {
        id: TabId,
        tab_group_id: Option<WorkspaceTabGroupId>,
        title: String,
        kind: String,
        raw: Value,
    },
}

impl WorkspaceSessionTab {
    /// 让 controller 在不解构每种 tab 私有字段时统一处理顺序、选中和分组。
    pub fn id(&self) -> &TabId {
        match self {
            Self::Terminal { id, .. }
            | Self::SftpTransfer { id, .. }
            | Self::WorkspaceFile { id, .. }
            | Self::Unsupported { id, .. } => id,
        }
    }

    /// 标签栏标题来自保存快照，空标题在 codec 入口直接拒绝。
    pub fn title(&self) -> &str {
        match self {
            Self::Terminal { title, .. }
            | Self::SftpTransfer { title, .. }
            | Self::WorkspaceFile { title, .. }
            | Self::Unsupported { title, .. } => title,
        }
    }

    /// 未知 tab 仍保留已有组关联，Root 不会通过类型 fallback 改变其排序。
    pub fn tab_group_id(&self) -> Option<&WorkspaceTabGroupId> {
        match self {
            Self::Terminal { tab_group_id, .. }
            | Self::SftpTransfer { tab_group_id, .. }
            | Self::WorkspaceFile { tab_group_id, .. }
            | Self::Unsupported { tab_group_id, .. } => tab_group_id.as_ref(),
        }
    }

    /// 只有 terminal tab 才有可渲染的 pane tree；文件类 tab 不制造空终端。
    pub fn terminal_layout(&self) -> Option<&TerminalLayoutNode> {
        match self {
            Self::Terminal { layout, .. } => Some(layout),
            _ => None,
        }
    }
}

/// 文件 tab 的只读/可编辑策略来自旧版 v3，未知权限值不得扩大写权限。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceFileAccess {
    Readonly,
    Editable,
}

/// 文件来源是已冻结的 v3 枚举；容器和 compose YAML 保留独立资源语义。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceFileSource {
    Sftp,
    Container,
    ComposeYaml,
    Workspace,
    Local,
}

/// 完整终端工作区的 typed 投影；shell/sidebar 等其他上下文仍留在原 JSON。
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceSessionSnapshot {
    /// 只接管可识别的固定容器；其它 sidebarMachines 行及扩展字段由文档原样保留。
    pub pinned_containers: Vec<WorkspacePinnedContainer>,
    pub active_tab_id: Option<TabId>,
    pub focused_pane_id: Option<PaneId>,
    pub selected_machine_id: Option<String>,
    pub terminal_tabs: Vec<WorkspaceSessionTab>,
    pub terminal_panes: Vec<WorkspaceSessionPane>,
    pub tab_groups: BTreeMap<WorkspaceTabGroupId, WorkspaceTabGroup>,
}

/// 侧栏容器只保存可重新解析的宿主与运行时身份，绝不保存连接凭据或一次性命令。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspacePinnedContainer {
    pub target: kerminal_domain::operations::ContainerTarget,
    pub name: String,
    pub description: String,
    pub group_id: Option<String>,
}

impl WorkspacePinnedContainer {
    /// 与 main targetStableId 一致，同一目标重复固定只更新展示信息。
    /// @author kongweiguang
    pub fn stable_id(&self) -> String {
        format!(
            "{}:{}:{}",
            match self.target.runtime {
                kerminal_domain::operations::ContainerRuntime::Docker => "docker",
                kerminal_domain::operations::ContainerRuntime::Podman => "podman",
            },
            self.target.host_id.as_ref().map_or("", HostId::as_str),
            self.target.container_id
        )
    }
}

impl WorkspaceSessionSnapshot {
    /// 持久化前集中验证 IDs、组引用和每个递归 layout，避免 Root/UI 缓存越界。
    /// @author kongweiguang
    pub fn validate(&self) -> Result<(), WorkspaceSessionInvalidReason> {
        let mut pinned_container_ids = BTreeSet::new();
        for container in &self.pinned_containers {
            if container.target.host_id.is_none() {
                return Err(WorkspaceSessionInvalidReason::InvalidSchema);
            }
            if container.target.container_id.is_empty()
                || container.target.container_id.chars().any(char::is_control)
                || container.group_id.as_ref().is_some_and(|group_id| {
                    group_id.is_empty() || group_id.chars().any(char::is_control)
                })
                || !pinned_container_ids.insert(container.stable_id())
            {
                return Err(WorkspaceSessionInvalidReason::InvalidSchema);
            }
        }

        let tab_ids = self
            .terminal_tabs
            .iter()
            .map(|tab| tab.id().clone())
            .collect::<BTreeSet<_>>();
        if tab_ids.len() != self.terminal_tabs.len() {
            return Err(WorkspaceSessionInvalidReason::InvalidSchema);
        }
        let pane_ids = self
            .terminal_panes
            .iter()
            .map(|pane| pane.id.clone())
            .collect::<BTreeSet<_>>();
        if pane_ids.len() != self.terminal_panes.len() {
            return Err(WorkspaceSessionInvalidReason::InvalidSchema);
        }
        if self
            .active_tab_id
            .as_ref()
            .is_some_and(|id| !tab_ids.contains(id))
        {
            return Err(WorkspaceSessionInvalidReason::InvalidSchema);
        }

        let mut referenced_panes = BTreeSet::new();
        for tab in &self.terminal_tabs {
            if tab
                .tab_group_id()
                .is_some_and(|group_id| !self.tab_groups.contains_key(group_id))
            {
                return Err(WorkspaceSessionInvalidReason::InvalidSchema);
            }
            if let Some(layout) = tab.terminal_layout() {
                layout
                    .validate()
                    .map_err(|_| WorkspaceSessionInvalidReason::InvalidSchema)?;
                for pane_id in layout.pane_ids() {
                    if !pane_ids.contains(&pane_id) || !referenced_panes.insert(pane_id) {
                        return Err(WorkspaceSessionInvalidReason::InvalidSchema);
                    }
                }
            }
        }

        if let Some(focused_pane_id) = &self.focused_pane_id {
            let active_layout = self
                .active_tab_id
                .as_ref()
                .and_then(|active_id| self.terminal_tabs.iter().find(|tab| tab.id() == active_id))
                .and_then(WorkspaceSessionTab::terminal_layout);
            if !active_layout.is_some_and(|layout| layout.contains_pane(focused_pane_id)) {
                return Err(WorkspaceSessionInvalidReason::InvalidSchema);
            }
        }
        Ok(())
    }
}

/// v3 document 保留原始 JSON；已知工作区字段通过 typed snapshot 读写。
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceSessionDocument {
    value: Value,
}

impl WorkspaceSessionDocument {
    /// 为首次创建提供与冻结 v3 同形的空基线，防止缺失状态伪装成旧 revision。
    pub fn empty_v3() -> Self {
        Self {
            value: json!({
                "version": 3,
                "activeTabId": "",
                "focusedPaneId": "",
                "selectedMachineId": "",
                "sidebarMachines": [],
                "terminalTabGroups": {},
                "terminalPanes": [],
                "terminalTabs": []
            }),
        }
    }

    /// 严格检查根版本和关键数组；密钥正文与损坏布局不能被包装成可覆盖快照。
    pub fn try_from_value(value: Value) -> Result<Self, WorkspaceSessionInvalidReason> {
        let object = value
            .as_object()
            .ok_or(WorkspaceSessionInvalidReason::InvalidRoot)?;
        if object.get("version").and_then(Value::as_u64) != Some(3) {
            return Err(WorkspaceSessionInvalidReason::InvalidSchema);
        }
        for field in ["sidebarMachines", "terminalPanes", "terminalTabs"] {
            if !object.get(field).is_some_and(Value::is_array) {
                return Err(WorkspaceSessionInvalidReason::InvalidSchema);
            }
        }
        if contains_credential_material(&value) {
            return Err(WorkspaceSessionInvalidReason::CredentialMaterial);
        }
        let document = Self { value };
        document.workspace_snapshot()?;
        Ok(document)
    }

    /// 暴露只读原文给仓储和诊断测试，UI 必须消费 typed snapshot。
    pub fn as_value(&self) -> &Value {
        &self.value
    }

    /// 将原文交给原子存储适配器，未被 codec 所有的根字段保持原样。
    pub fn into_value(self) -> Value {
        self.value
    }

    /// 解码全部 v3 tab、pane、target、组、活动项与布局，不静默丢弃损坏行。
    pub fn workspace_snapshot(
        &self,
    ) -> Result<WorkspaceSessionSnapshot, WorkspaceSessionInvalidReason> {
        decode_workspace_snapshot(&self.value)
    }

    /// 更新 typed 所有字段并按稳定 ID 合并原行，从而保留行内和递归布局未知字段。
    /// 固定容器沿用主机侧栏身份和移除标记，重排时始终紧随父 SSH 主机且保持同宿主顺序。
    ///
    /// @author kongweiguang
    pub fn update_known_fields(
        &mut self,
        snapshot: &WorkspaceSessionSnapshot,
    ) -> Result<(), WorkspaceSessionInvalidReason> {
        snapshot.validate()?;
        let mut next = self.value.clone();
        let root = next
            .as_object_mut()
            .ok_or(WorkspaceSessionInvalidReason::InvalidRoot)?;
        update_pinned_sidebar(root, snapshot)?;
        let old_tabs = index_rows(root.get("terminalTabs"))?;
        let old_panes = index_rows(root.get("terminalPanes"))?;
        let old_groups = root
            .get("terminalTabGroups")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        let tabs = snapshot
            .terminal_tabs
            .iter()
            .map(|tab| merge_tab(old_tabs.get(tab.id().as_str()), tab))
            .collect::<Result<Vec<_>, _>>()?;
        let panes = snapshot
            .terminal_panes
            .iter()
            .map(|pane| merge_pane(old_panes.get(pane.id.as_str()), pane))
            .collect::<Result<Vec<_>, _>>()?;
        let groups = snapshot
            .tab_groups
            .iter()
            .map(|(id, group)| {
                let raw = old_groups.get(id.as_str());
                (id.as_str().to_owned(), merge_group(raw, group))
            })
            .collect::<Map<_, _>>();

        root.insert("terminalTabs".to_owned(), Value::Array(tabs));
        root.insert("terminalPanes".to_owned(), Value::Array(panes));
        root.insert("terminalTabGroups".to_owned(), Value::Object(groups));
        root.insert(
            "activeTabId".to_owned(),
            Value::String(
                snapshot
                    .active_tab_id
                    .as_ref()
                    .map_or_else(String::new, |id| id.as_str().to_owned()),
            ),
        );
        root.insert(
            "focusedPaneId".to_owned(),
            Value::String(
                snapshot
                    .focused_pane_id
                    .as_ref()
                    .map_or_else(String::new, |id| id.as_str().to_owned()),
            ),
        );
        root.insert(
            "selectedMachineId".to_owned(),
            Value::String(snapshot.selected_machine_id.clone().unwrap_or_default()),
        );
        Self::try_from_value(next.clone())?;
        self.value = next;
        Ok(())
    }

    /// 兼容布局用例只返回 terminal tabs；文件和传输 tab 不生成伪布局。
    pub fn terminal_layouts(
        &self,
    ) -> Result<Vec<(TabId, TerminalLayoutNode)>, WorkspaceSessionInvalidReason> {
        Ok(self
            .workspace_snapshot()?
            .terminal_tabs
            .into_iter()
            .filter_map(|tab| match tab {
                WorkspaceSessionTab::Terminal { id, layout, .. } => Some((id, layout)),
                _ => None,
            })
            .collect())
    }
}

/// 解码根部已知工作区字段并校验跨表引用；其他 v3 根字段保持完全不透明。
///
/// @author kongweiguang
fn decode_workspace_snapshot(
    value: &Value,
) -> Result<WorkspaceSessionSnapshot, WorkspaceSessionInvalidReason> {
    let root = value
        .as_object()
        .ok_or(WorkspaceSessionInvalidReason::InvalidRoot)?;
    let panes = root
        .get("terminalPanes")
        .and_then(Value::as_array)
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?
        .iter()
        .map(decode_pane)
        .collect::<Result<Vec<_>, _>>()?;
    let groups = decode_groups(root.get("terminalTabGroups"))?;
    let tabs = root
        .get("terminalTabs")
        .and_then(Value::as_array)
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?
        .iter()
        .map(decode_tab)
        .collect::<Result<Vec<_>, _>>()?;
    let snapshot = WorkspaceSessionSnapshot {
        pinned_containers: root
            .get("sidebarMachines")
            .and_then(Value::as_array)
            .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?
            .iter()
            .map(decode_pinned_container)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect(),
        active_tab_id: decode_optional_identity::<TabId>(root.get("activeTabId"), TabId::parse)?,
        focused_pane_id: decode_optional_identity::<PaneId>(
            root.get("focusedPaneId"),
            PaneId::parse,
        )?,
        selected_machine_id: optional_string(root.get("selectedMachineId"))?,
        terminal_tabs: tabs,
        terminal_panes: panes,
        tab_groups: groups,
    };
    snapshot.validate()?;
    Ok(snapshot)
}

/// 合并固定容器侧栏并维护旧 schema 的移除标记；未知行原样保留，缺失父行时也不丢弃 pin。
///
/// @author kongweiguang
fn update_pinned_sidebar(
    root: &mut Map<String, Value>,
    snapshot: &WorkspaceSessionSnapshot,
) -> Result<(), WorkspaceSessionInvalidReason> {
    let old_sidebar = root
        .get("sidebarMachines")
        .and_then(Value::as_array)
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
    let mut retained_rows = Vec::with_capacity(old_sidebar.len());
    let mut old_pins = BTreeMap::new();
    for row in old_sidebar {
        if let Some(pin) = decode_pinned_container(row)? {
            old_pins.insert(pin.stable_id(), (pin, row.clone()));
        } else {
            retained_rows.push(row.clone());
        }
    }

    let had_removed = root.contains_key("removedSidebarMachineIds");
    let mut removed = match root.get("removedSidebarMachineIds") {
        Some(value) => value
            .as_array()
            .cloned()
            .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?,
        None => Vec::new(),
    };
    let previous_removed = removed.clone();
    let desired_ids = snapshot
        .pinned_containers
        .iter()
        .map(WorkspacePinnedContainer::stable_id)
        .collect::<BTreeSet<_>>();
    for id in old_pins.keys().filter(|id| !desired_ids.contains(*id)) {
        if !removed
            .iter()
            .any(|value| value.as_str() == Some(id.as_str()))
        {
            removed.push(Value::String(id.clone()));
        }
    }
    removed.retain(|value| value.as_str().is_none_or(|id| !desired_ids.contains(id)));

    let mut pins_by_host: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for container in &snapshot.pinned_containers {
        let id = container.stable_id();
        let host_id = container
            .target
            .host_id
            .as_ref()
            .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
        let host_id_str = host_id.as_str();
        let runtime = match container.target.runtime {
            kerminal_domain::operations::ContainerRuntime::Docker => "docker",
            kerminal_domain::operations::ContainerRuntime::Podman => "podman",
        };
        let mut row = old_pins
            .get(&id)
            .map(|(_, row)| row.clone())
            .unwrap_or_else(|| json!({}));
        let fields = row
            .as_object_mut()
            .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
        fields.extend(
            json!({
                "id": id,
                "kind": "dockerContainer",
                "name": container.name,
                "description": container.description,
                "parentMachineId": host_id_str,
                "containerId": container.target.container_id,
                "containerName": container.name,
                "runtime": runtime,
                "status": "offline",
                "tags": ["container", runtime],
                "target": {
                    "kind": "dockerContainer",
                    "hostId": host_id_str,
                    "containerId": container.target.container_id,
                    "containerName": container.name,
                    "runtime": runtime
                }
            })
            .as_object()
            .expect("object literal")
            .clone(),
        );
        set_optional_string(fields, "remoteGroupId", container.group_id.as_deref());
        pins_by_host
            .entry(host_id_str.to_owned())
            .or_default()
            .push(row);
    }

    let mut sidebar = Vec::with_capacity(retained_rows.len() + snapshot.pinned_containers.len());
    for row in retained_rows {
        let parent_id = row.get("id").and_then(Value::as_str).map(str::to_owned);
        sidebar.push(row);
        if let Some(children) = parent_id.and_then(|id| pins_by_host.remove(&id)) {
            sidebar.extend(children);
        }
    }
    for (_, children) in pins_by_host {
        sidebar.extend(children);
    }
    root.insert("sidebarMachines".to_owned(), Value::Array(sidebar));
    if had_removed || removed != previous_removed {
        root.insert("removedSidebarMachineIds".to_owned(), Value::Array(removed));
    }
    Ok(())
}

/// 保留未识别侧栏行，同时严格拒绝损坏的已知容器行，避免静默丢失用户配置。
///
/// @author kongweiguang
fn decode_pinned_container(
    row: &Value,
) -> Result<Option<WorkspacePinnedContainer>, WorkspaceSessionInvalidReason> {
    let Some(object) = row.as_object() else {
        return Ok(None);
    };
    if object.get("kind").and_then(Value::as_str) != Some("dockerContainer") {
        return Ok(None);
    }
    let target = decode_target(
        object
            .get("target")
            .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?,
    )?;
    let target = match target {
        WorkspaceTargetRef::DockerContainer {
            host_id,
            container_id,
            runtime,
            ..
        } => kerminal_domain::operations::ContainerTarget {
            host_id: Some(host_id),
            container_id,
            runtime: match runtime {
                WorkspaceContainerRuntime::Docker => {
                    kerminal_domain::operations::ContainerRuntime::Docker
                }
                WorkspaceContainerRuntime::Podman => {
                    kerminal_domain::operations::ContainerRuntime::Podman
                }
            },
        },
        _ => return Err(WorkspaceSessionInvalidReason::InvalidSchema),
    };
    let id = required_string(object.get("id"))?;
    let parent_id = required_string(object.get("parentMachineId"))?;
    let host_id = target
        .host_id
        .as_ref()
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
    let runtime = match target.runtime {
        kerminal_domain::operations::ContainerRuntime::Docker => "docker",
        kerminal_domain::operations::ContainerRuntime::Podman => "podman",
    };
    if id != format!("{runtime}:{}:{}", host_id.as_str(), target.container_id)
        || parent_id != host_id.as_str()
    {
        return Err(WorkspaceSessionInvalidReason::InvalidSchema);
    }
    Ok(Some(WorkspacePinnedContainer {
        target,
        name: required_string(object.get("name"))?,
        description: optional_string(object.get("description"))?.unwrap_or_default(),
        group_id: optional_string(object.get("remoteGroupId"))?,
    }))
}

/// 解码组 definitions；缺省 v3 字段视为空映射，非对象结构则拒绝保存。
fn decode_groups(
    value: Option<&Value>,
) -> Result<BTreeMap<WorkspaceTabGroupId, WorkspaceTabGroup>, WorkspaceSessionInvalidReason> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let object = value
        .as_object()
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
    object
        .iter()
        .map(|(raw_id, value)| {
            let id = WorkspaceTabGroupId::parse(raw_id)?;
            let fields = value
                .as_object()
                .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
            let title = required_string(fields.get("title"))?;
            let color = fields.get("color").map(decode_group_color).transpose()?;
            let collapsed = optional_bool(fields.get("collapsed"))?.unwrap_or(false);
            Ok((
                id,
                WorkspaceTabGroup {
                    title,
                    color,
                    collapsed,
                },
            ))
        })
        .collect()
}

/// 未知颜色先保留其稳定字符串，避免会话保存时把用户自定义主题写成默认值。
fn decode_group_color(
    value: &Value,
) -> Result<WorkspaceTabGroupColor, WorkspaceSessionInvalidReason> {
    let raw = value
        .as_str()
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
    Ok(match raw {
        "blue" => WorkspaceTabGroupColor::Blue,
        "pink" => WorkspaceTabGroupColor::Pink,
        "purple" => WorkspaceTabGroupColor::Purple,
        "mint" => WorkspaceTabGroupColor::Mint,
        "amber" => WorkspaceTabGroupColor::Amber,
        "teal" => WorkspaceTabGroupColor::Teal,
        "orange" => WorkspaceTabGroupColor::Orange,
        "gray" => WorkspaceTabGroupColor::Gray,
        unknown => WorkspaceTabGroupColor::Unsupported(unknown.to_owned()),
    })
}

/// 解码全部 pane 连接字段；当前读取不再补旧缺省，避免掩盖损坏，显式空 prompt 仍须保真。
fn decode_pane(value: &Value) -> Result<WorkspaceSessionPane, WorkspaceSessionInvalidReason> {
    let object = value
        .as_object()
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
    let id = parse_pane_id(object.get("id"))?;
    let title = required_string(object.get("title"))?;
    let machine_id = required_string(object.get("machineId"))?;
    let mode = decode_pane_mode(object.get("mode"))?;
    let target = object
        .get("target")
        .filter(|value| !value.is_null())
        .map(decode_target)
        .transpose()?;
    let remote_host_id = optional_host_id(object.get("remoteHostId"))?;
    let container_id = optional_string(object.get("containerId"))?;
    let profile_id = optional_string(object.get("profileId"))?;
    let shell = optional_string(object.get("shell"))?;
    let args = optional_string_array(object.get("args"))?;
    let cwd = optional_string(object.get("cwd"))?;
    let current_cwd = optional_string(object.get("currentCwd"))?;
    let env = optional_string_map(object.get("env"))?;
    let remote_command = optional_string(object.get("remoteCommand"))?;
    let tmux_binding = object
        .get("tmuxBinding")
        .filter(|value| !value.is_null())
        .map(decode_tmux_binding)
        .transpose()?;
    let prompt = decode_prompt(object.get("prompt"))?;
    let status = decode_status(object.get("status"))?;
    let latency_ms = optional_f64(object.get("latencyMs"))?;
    let local_machine_scope = decode_local_scope(object.get("localMachineScope"))?;
    let output_history = optional_string(object.get("outputHistory"))?;
    Ok(WorkspaceSessionPane {
        id,
        title,
        machine_id,
        mode,
        target,
        remote_host_id,
        container_id,
        profile_id,
        shell,
        args,
        cwd,
        current_cwd,
        env,
        remote_command,
        tmux_binding,
        prompt,
        status,
        latency_ms,
        local_machine_scope,
        output_history,
    })
}

/// 解码 frozen v3 的五种 target kind；未知/失效目标保留原文但不可恢复启动。
fn decode_target(value: &Value) -> Result<WorkspaceTargetRef, WorkspaceSessionInvalidReason> {
    let Some(object) = value.as_object() else {
        return Ok(unavailable_target(
            value,
            WorkspaceTargetUnavailableReason::Malformed,
        ));
    };
    let Some(kind) = object.get("kind").and_then(Value::as_str) else {
        return Ok(unavailable_target(
            value,
            WorkspaceTargetUnavailableReason::Malformed,
        ));
    };
    let invalid_host =
        || unavailable_target(value, WorkspaceTargetUnavailableReason::InvalidHostId);
    match kind {
        "local" => Ok(WorkspaceTargetRef::Local {
            profile_id: optional_string(object.get("profileId"))?,
        }),
        "ssh" | "telnet" | "serial" => {
            let Some(raw_host) = object.get("hostId").and_then(Value::as_str) else {
                return Ok(unavailable_target(
                    value,
                    WorkspaceTargetUnavailableReason::InvalidHostId,
                ));
            };
            let Ok(host_id) = HostId::parse(raw_host) else {
                return Ok(invalid_host());
            };
            Ok(match kind {
                "ssh" => WorkspaceTargetRef::Ssh { host_id },
                "telnet" => WorkspaceTargetRef::Telnet { host_id },
                _ => WorkspaceTargetRef::Serial { host_id },
            })
        }
        "dockerContainer" => {
            let Some(raw_host) = object.get("hostId").and_then(Value::as_str) else {
                return Ok(invalid_host());
            };
            let Ok(host_id) = HostId::parse(raw_host) else {
                return Ok(invalid_host());
            };
            let Some(container_id) = object.get("containerId").and_then(Value::as_str) else {
                return Ok(unavailable_target(
                    value,
                    WorkspaceTargetUnavailableReason::MissingContainerId,
                ));
            };
            if container_id.is_empty() || container_id.chars().any(char::is_control) {
                return Ok(unavailable_target(
                    value,
                    WorkspaceTargetUnavailableReason::MissingContainerId,
                ));
            }
            let runtime = match object
                .get("runtime")
                .and_then(Value::as_str)
                .unwrap_or("docker")
            {
                "docker" => WorkspaceContainerRuntime::Docker,
                "podman" => WorkspaceContainerRuntime::Podman,
                _ => {
                    return Ok(unavailable_target(
                        value,
                        WorkspaceTargetUnavailableReason::UnknownRuntime,
                    ));
                }
            };
            Ok(WorkspaceTargetRef::DockerContainer {
                host_id,
                container_id: container_id.to_owned(),
                runtime,
                container_name: optional_string(object.get("containerName"))?,
                user: optional_string(object.get("user"))?,
                workdir: optional_string(object.get("workdir"))?,
            })
        }
        _ => Ok(unavailable_target(
            value,
            WorkspaceTargetUnavailableReason::UnknownKind,
        )),
    }
}

/// 把不可用 target 的原始 JSON 留给显式修复流程，禁止按 pane mode 猜测目标。
fn unavailable_target(
    value: &Value,
    reason: WorkspaceTargetUnavailableReason,
) -> WorkspaceTargetRef {
    WorkspaceTargetRef::Unavailable {
        raw: value.clone(),
        reason,
    }
}

/// 解码 terminal/sftp/file tab；缺失的初始本地目录按旧版语义留空，未知 kind 保持 opaque。
fn decode_tab(value: &Value) -> Result<WorkspaceSessionTab, WorkspaceSessionInvalidReason> {
    let object = value
        .as_object()
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
    let id = parse_tab_id(object.get("id"))?;
    let title = required_string(object.get("title"))?;
    let tab_group_id = object
        .get("tabGroupId")
        .filter(|value| !value.is_null())
        .map(parse_group_id_value)
        .transpose()?;
    let machine_id = optional_string(object.get("machineId"))?;
    match object.get("kind").and_then(Value::as_str) {
        None | Some("terminal") => {
            let machine_id = machine_id.ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
            let layout = object
                .get("layout")
                .cloned()
                .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
            let layout: TerminalLayoutNode = serde_json::from_value(layout)
                .map_err(|_| WorkspaceSessionInvalidReason::InvalidSchema)?;
            layout
                .validate()
                .map_err(|_| WorkspaceSessionInvalidReason::InvalidSchema)?;
            Ok(WorkspaceSessionTab::Terminal {
                id,
                tab_group_id,
                title,
                machine_id,
                layout,
            })
        }
        Some("sftpTransfer") => Ok(WorkspaceSessionTab::SftpTransfer {
            id,
            tab_group_id,
            title,
            machine_id: machine_id.ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?,
            left_host_id: optional_host_id(object.get("leftHostId"))?,
            locked_left_host_id: optional_host_id(object.get("lockedLeftHostId"))?,
            right_host_id: optional_host_id(object.get("rightHostId"))?,
            external_launch_id: optional_string(object.get("externalLaunchId"))?,
            initial_left_path: optional_string(object.get("initialLeftPath"))?,
            initial_right_path: optional_string(object.get("initialRightPath"))?,
            initial_right_selection: optional_string(object.get("initialRightSelection"))?,
        }),
        Some("workspaceFile") => Ok(WorkspaceSessionTab::WorkspaceFile {
            id,
            tab_group_id,
            title,
            machine_id: machine_id.ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?,
            target: object
                .get("target")
                .map(decode_target)
                .transpose()?
                .unwrap_or_else(|| {
                    unavailable_target(&Value::Null, WorkspaceTargetUnavailableReason::Malformed)
                }),
            path: required_string(object.get("path"))?,
            root_path: optional_string(object.get("rootPath"))?,
            access: decode_file_access(object.get("access"))?,
            source: decode_file_source(object.get("source"))?,
        }),
        Some(kind) => Ok(WorkspaceSessionTab::Unsupported {
            id,
            tab_group_id,
            title,
            kind: kind.to_owned(),
            raw: value.clone(),
        }),
    }
}

/// 未知 tab 关联的分组也须满足稳定 ID 约束，避免拖拽组件接收空身份。
fn parse_group_id_value(
    value: &Value,
) -> Result<WorkspaceTabGroupId, WorkspaceSessionInvalidReason> {
    WorkspaceTabGroupId::parse(
        value
            .as_str()
            .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?,
    )
}

/// 严格读取 pane mode，避免把不明协议默认为本地 shell。
fn decode_pane_mode(
    value: Option<&Value>,
) -> Result<WorkspacePaneMode, WorkspaceSessionInvalidReason> {
    match value.and_then(Value::as_str) {
        Some("local") => Ok(WorkspacePaneMode::Local),
        Some("ssh") => Ok(WorkspacePaneMode::Ssh),
        Some("telnet") => Ok(WorkspacePaneMode::Telnet),
        Some("serial") => Ok(WorkspacePaneMode::Serial),
        Some("container") => Ok(WorkspacePaneMode::Container),
        Some("preview") => Ok(WorkspacePaneMode::Preview),
        _ => Err(WorkspaceSessionInvalidReason::InvalidSchema),
    }
}

/// v3 access 只允许只读或编辑两种明确状态。
fn decode_file_access(
    value: Option<&Value>,
) -> Result<WorkspaceFileAccess, WorkspaceSessionInvalidReason> {
    match value.and_then(Value::as_str) {
        Some("readonly") => Ok(WorkspaceFileAccess::Readonly),
        Some("editable") => Ok(WorkspaceFileAccess::Editable),
        _ => Err(WorkspaceSessionInvalidReason::InvalidSchema),
    }
}

/// v3 文件来源白名单防止未知来源被错误路由到可写 adapter。
fn decode_file_source(
    value: Option<&Value>,
) -> Result<WorkspaceFileSource, WorkspaceSessionInvalidReason> {
    match value.and_then(Value::as_str) {
        Some("sftp") => Ok(WorkspaceFileSource::Sftp),
        Some("container") => Ok(WorkspaceFileSource::Container),
        Some("composeYaml") => Ok(WorkspaceFileSource::ComposeYaml),
        Some("workspace") => Ok(WorkspaceFileSource::Workspace),
        Some("local") => Ok(WorkspaceFileSource::Local),
        _ => Err(WorkspaceSessionInvalidReason::InvalidSchema),
    }
}

/// 当前 v3 必须显式声明已知状态；旧缺省值只由一次性迁移负责补齐。
fn decode_status(
    value: Option<&Value>,
) -> Result<WorkspaceMachineStatus, WorkspaceSessionInvalidReason> {
    match value {
        Some(Value::String(status)) => match status.as_str() {
            "online" => Ok(WorkspaceMachineStatus::Online),
            "offline" => Ok(WorkspaceMachineStatus::Offline),
            "warning" => Ok(WorkspaceMachineStatus::Warning),
            _ => Err(WorkspaceSessionInvalidReason::InvalidSchema),
        },
        _ => Err(WorkspaceSessionInvalidReason::InvalidSchema),
    }
}

/// localMachineScope 缺失/null 表示当前可选语义，未知值不能伪装成 sidebar 权限。
fn decode_local_scope(
    value: Option<&Value>,
) -> Result<Option<WorkspaceLocalMachineScope>, WorkspaceSessionInvalidReason> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(scope)) => match scope.as_str() {
            "workspace" => Ok(Some(WorkspaceLocalMachineScope::Workspace)),
            "sidebar" => Ok(Some(WorkspaceLocalMachineScope::Sidebar)),
            _ => Err(WorkspaceSessionInvalidReason::InvalidSchema),
        },
        _ => Err(WorkspaceSessionInvalidReason::InvalidSchema),
    }
}

/// tmux 缺少完整绑定时不启动半绑定资源，v3 原行仍可由 codec 保留其额外 JSON。
fn decode_tmux_binding(
    value: &Value,
) -> Result<WorkspaceTmuxBinding, WorkspaceSessionInvalidReason> {
    let object = value
        .as_object()
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
    Ok(WorkspaceTmuxBinding {
        target_ref: required_string(object.get("targetRef"))?,
        session_id: required_string(object.get("sessionId"))?,
        session_name: required_string(object.get("sessionName"))?,
        attached_at: required_string(object.get("attachedAt"))?,
        socket_name: optional_string(object.get("socketName"))?,
        socket_path: optional_string(object.get("socketPath"))?,
    })
}

/// 泛型解析 v3 的强类型 opaque ID，统一拒绝空值和控制字符。
fn parse_identity<T>(
    value: Option<&Value>,
    parse: impl FnOnce(&str) -> Result<T, kerminal_domain::DomainError>,
) -> Result<T, WorkspaceSessionInvalidReason> {
    let raw = value
        .and_then(Value::as_str)
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
    parse(raw).map_err(|_| WorkspaceSessionInvalidReason::InvalidSchema)
}

/// Tab ID 使用独立领域类型，不能与 PaneId 或 HostId 互换。
fn parse_tab_id(value: Option<&Value>) -> Result<TabId, WorkspaceSessionInvalidReason> {
    parse_identity(value, TabId::parse)
}

/// Pane ID 使用独立领域类型，避免布局引用误指向任意字符串。
fn parse_pane_id(value: Option<&Value>) -> Result<PaneId, WorkspaceSessionInvalidReason> {
    parse_identity(value, PaneId::parse)
}

/// Host ID 与 tab/pane ID 分离，连接目标只接受可持久化 host identity。
fn optional_host_id(
    value: Option<&Value>,
) -> Result<Option<HostId>, WorkspaceSessionInvalidReason> {
    value
        .filter(|value| !value.is_null())
        .map(|value| parse_identity(Some(value), HostId::parse))
        .transpose()
}

/// 空字符串与缺省字段都表示未选择；非空值必须是有效领域 ID。
fn decode_optional_identity<T>(
    value: Option<&Value>,
    parse: impl FnOnce(&str) -> Result<T, kerminal_domain::DomainError>,
) -> Result<Option<T>, WorkspaceSessionInvalidReason> {
    let Some(raw) = value.and_then(Value::as_str) else {
        return if value.is_none() || value.is_some_and(Value::is_null) {
            Ok(None)
        } else {
            Err(WorkspaceSessionInvalidReason::InvalidSchema)
        };
    };
    if raw.is_empty() {
        return Ok(None);
    }
    parse(raw)
        .map(Some)
        .map_err(|_| WorkspaceSessionInvalidReason::InvalidSchema)
}

/// 非空业务字符串需要有效 UTF-8 且不能是空白占位。
fn required_string(value: Option<&Value>) -> Result<String, WorkspaceSessionInvalidReason> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)
}

/// 可选字符串只接受字符串或 null，避免损坏类型被编码为空字符串覆盖。
fn optional_string(value: Option<&Value>) -> Result<Option<String>, WorkspaceSessionInvalidReason> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(Value::String(_)) => Ok(None),
        _ => Err(WorkspaceSessionInvalidReason::InvalidSchema),
    }
}

/// 当前 v3 必须显式提供字符串；空字符串仍是合法状态，旧缺省值只由一次性迁移补入。
fn decode_prompt(value: Option<&Value>) -> Result<String, WorkspaceSessionInvalidReason> {
    match value {
        Some(Value::String(prompt)) => Ok(prompt.clone()),
        _ => Err(WorkspaceSessionInvalidReason::InvalidSchema),
    }
}

/// 可选 bool 供旧版缺省 collapsed 使用，其余错误类型不能伪装成 false。
fn optional_bool(value: Option<&Value>) -> Result<Option<bool>, WorkspaceSessionInvalidReason> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        _ => Err(WorkspaceSessionInvalidReason::InvalidSchema),
    }
}

/// 仅接受有限的 layout/终端时延数值，拒绝 NaN 与 Infinity。
fn optional_f64(value: Option<&Value>) -> Result<Option<f64>, WorkspaceSessionInvalidReason> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_f64()
            .filter(|value| value.is_finite())
            .map(Some)
            .ok_or(WorkspaceSessionInvalidReason::InvalidSchema),
        _ => Err(WorkspaceSessionInvalidReason::InvalidSchema),
    }
}

/// v3 args 必须是纯字符串数组，防止命令参数被静默重排或丢弃。
fn optional_string_array(
    value: Option<&Value>,
) -> Result<Option<Vec<String>>, WorkspaceSessionInvalidReason> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        _ => Err(WorkspaceSessionInvalidReason::InvalidSchema),
    }
}

/// 环境变量只接受 string->string，避免执行 shell 时将结构化 JSON 转成隐式文本。
fn optional_string_map(
    value: Option<&Value>,
) -> Result<Option<BTreeMap<String, String>>, WorkspaceSessionInvalidReason> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let object = value
        .as_object()
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
    object
        .iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.clone(), value.to_owned()))
                .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)
        })
        .collect::<Result<BTreeMap<_, _>, _>>()
        .map(Some)
}

/// 建立 ID 到原始 JSON 行的索引，后续 merge 才能保留未知行内字段。
fn index_rows(
    value: Option<&Value>,
) -> Result<BTreeMap<String, Value>, WorkspaceSessionInvalidReason> {
    let rows = value
        .and_then(Value::as_array)
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
    let mut indexed = BTreeMap::new();
    for row in rows {
        let id = row
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
        if indexed.insert(id.to_owned(), row.clone()).is_some() {
            return Err(WorkspaceSessionInvalidReason::InvalidSchema);
        }
    }
    Ok(indexed)
}

/// 复用已存在 tab 的原始对象，只改 v3 已知标签字段和终端 layout；可选目录不擦除未知扩展。
fn merge_tab(
    existing: Option<&Value>,
    tab: &WorkspaceSessionTab,
) -> Result<Value, WorkspaceSessionInvalidReason> {
    if let WorkspaceSessionTab::Unsupported {
        id,
        tab_group_id,
        raw,
        ..
    } = tab
    {
        let mut row = raw.clone();
        let object = row
            .as_object_mut()
            .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
        object.insert("id".to_owned(), Value::String(id.as_str().to_owned()));
        set_optional_string(
            object,
            "tabGroupId",
            tab_group_id.as_ref().map(WorkspaceTabGroupId::as_str),
        );
        return Ok(row);
    }
    let mut row = existing
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let object = row
        .as_object_mut()
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
    match tab {
        WorkspaceSessionTab::Terminal {
            id,
            tab_group_id,
            title,
            machine_id,
            layout,
        } => {
            set_common_tab_fields(object, id, tab_group_id.as_ref(), title, Some(machine_id));
            let old_layout = object.get("layout");
            object.insert("layout".to_owned(), merge_layout(old_layout, layout));
        }
        WorkspaceSessionTab::SftpTransfer {
            id,
            tab_group_id,
            title,
            machine_id,
            left_host_id,
            locked_left_host_id,
            right_host_id,
            external_launch_id,
            initial_left_path,
            initial_right_path,
            initial_right_selection,
        } => {
            set_common_tab_fields(object, id, tab_group_id.as_ref(), title, Some(machine_id));
            object.insert("kind".to_owned(), Value::String("sftpTransfer".to_owned()));
            set_optional_string(
                object,
                "leftHostId",
                left_host_id.as_ref().map(HostId::as_str),
            );
            set_optional_string(
                object,
                "lockedLeftHostId",
                locked_left_host_id.as_ref().map(HostId::as_str),
            );
            set_optional_string(
                object,
                "rightHostId",
                right_host_id.as_ref().map(HostId::as_str),
            );
            set_optional_string(object, "externalLaunchId", external_launch_id.as_deref());
            set_optional_string(object, "initialLeftPath", initial_left_path.as_deref());
            set_optional_string(object, "initialRightPath", initial_right_path.as_deref());
            set_optional_string(
                object,
                "initialRightSelection",
                initial_right_selection.as_deref(),
            );
        }
        WorkspaceSessionTab::WorkspaceFile {
            id,
            tab_group_id,
            title,
            machine_id,
            target,
            path,
            root_path,
            access,
            source,
        } => {
            set_common_tab_fields(object, id, tab_group_id.as_ref(), title, Some(machine_id));
            object.insert("kind".to_owned(), Value::String("workspaceFile".to_owned()));
            let target_value = merge_target(object.get("target"), target);
            object.insert("target".to_owned(), target_value);
            object.insert("path".to_owned(), Value::String(path.clone()));
            set_optional_string(object, "rootPath", root_path.as_deref());
            object.insert(
                "access".to_owned(),
                Value::String(file_access_str(*access).to_owned()),
            );
            object.insert(
                "source".to_owned(),
                Value::String(file_source_str(*source).to_owned()),
            );
        }
        WorkspaceSessionTab::Unsupported { raw, .. } => return Ok(raw.clone()),
    }
    Ok(row)
}

/// 同一公共更新点写入 ID/title/group，成员顺序仍由 terminalTabs 数组负责。
fn set_common_tab_fields(
    object: &mut Map<String, Value>,
    id: &TabId,
    group: Option<&WorkspaceTabGroupId>,
    title: &str,
    machine_id: Option<&String>,
) {
    object.insert("id".to_owned(), Value::String(id.as_str().to_owned()));
    object.insert("title".to_owned(), Value::String(title.to_owned()));
    set_optional_string(object, "tabGroupId", group.map(WorkspaceTabGroupId::as_str));
    set_optional_string(object, "machineId", machine_id.map(String::as_str));
}

/// 合并递归 layout 节点时按 paneId/splitId 匹配，保留其未知扩展字段。
fn merge_layout(existing: Option<&Value>, layout: &TerminalLayoutNode) -> Value {
    let mut node = existing
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let Some(object) = node.as_object_mut() else {
        return node;
    };
    match layout {
        TerminalLayoutNode::Pane { pane_id } => {
            object.insert("type".to_owned(), Value::String("pane".to_owned()));
            object.insert(
                "paneId".to_owned(),
                Value::String(pane_id.as_str().to_owned()),
            );
        }
        TerminalLayoutNode::Split {
            id,
            direction,
            children,
            sizes,
        } => {
            object.insert("type".to_owned(), Value::String("split".to_owned()));
            object.insert("id".to_owned(), Value::String(id.clone()));
            object.insert(
                "direction".to_owned(),
                Value::String(split_direction_str(*direction).to_owned()),
            );
            let old_children = object
                .get("children")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let by_key = old_children
                .iter()
                .filter_map(|child| layout_node_key(child).map(|key| (key, child)))
                .collect::<BTreeMap<_, _>>();
            let next_children = children
                .iter()
                .map(|child| {
                    let key = terminal_layout_key(child);
                    merge_layout(by_key.get(&key).copied(), child)
                })
                .collect();
            object.insert("children".to_owned(), Value::Array(next_children));
            if let Some(sizes) = sizes {
                object.insert(
                    "sizes".to_owned(),
                    Value::Object(
                        sizes
                            .iter()
                            .map(|(key, value)| (key.clone(), json!(value)))
                            .collect(),
                    ),
                );
            } else {
                object.remove("sizes");
            }
        }
    }
    node
}

/// split 的 sizes 键使用直接 child 的稳定 ID，不按数组下标重建比例。
fn layout_node_key(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    match object.get("type")?.as_str()? {
        "pane" => object
            .get("paneId")?
            .as_str()
            .map(|id| format!("pane:{id}")),
        "split" => object.get("id")?.as_str().map(|id| format!("split:{id}")),
        _ => None,
    }
}

/// typed layout 的匹配键与旧 JSON 节点键保持同一前缀约定。
fn terminal_layout_key(node: &TerminalLayoutNode) -> String {
    match node {
        TerminalLayoutNode::Pane { pane_id } => format!("pane:{}", pane_id.as_str()),
        TerminalLayoutNode::Split { id, .. } => format!("split:{id}"),
    }
}

/// 按 groupId 保留原定义的未知字段，只更新标题、颜色和折叠状态。
fn merge_group(existing: Option<&Value>, group: &WorkspaceTabGroup) -> Value {
    let mut value = existing
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let Some(object) = value.as_object_mut() else {
        return value;
    };
    object.insert("title".to_owned(), Value::String(group.title.clone()));
    object.insert("collapsed".to_owned(), Value::Bool(group.collapsed));
    if let Some(color) = &group.color {
        object.insert(
            "color".to_owned(),
            Value::String(group_color_str(color).to_owned()),
        );
    } else {
        object.remove("color");
    }
    value
}

/// Pane 行按 ID 原位 merge，未被 UI 修改的老输出/扩展字段不被重建丢弃。
fn merge_pane(
    existing: Option<&Value>,
    pane: &WorkspaceSessionPane,
) -> Result<Value, WorkspaceSessionInvalidReason> {
    if existing.is_none() {
        return Ok(encode_new_pane(pane));
    }
    let mut row = existing
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let object = row
        .as_object_mut()
        .ok_or(WorkspaceSessionInvalidReason::InvalidSchema)?;
    object.insert("id".to_owned(), Value::String(pane.id.as_str().to_owned()));
    object.insert("title".to_owned(), Value::String(pane.title.clone()));
    object.insert(
        "machineId".to_owned(),
        Value::String(pane.machine_id.clone()),
    );
    object.insert(
        "mode".to_owned(),
        Value::String(pane_mode_str(pane.mode).to_owned()),
    );
    if let Some(target) = &pane.target {
        let value = merge_target(object.get("target"), target);
        object.insert("target".to_owned(), value);
    } else {
        object.remove("target");
    }
    set_optional_string(
        object,
        "remoteHostId",
        pane.remote_host_id.as_ref().map(HostId::as_str),
    );
    set_optional_string(object, "containerId", pane.container_id.as_deref());
    set_optional_string(object, "profileId", pane.profile_id.as_deref());
    set_optional_string(object, "shell", pane.shell.as_deref());
    set_optional_string_array(object, "args", pane.args.as_deref());
    set_optional_string(object, "cwd", pane.cwd.as_deref());
    set_optional_string(object, "currentCwd", pane.current_cwd.as_deref());
    set_optional_string_map(object, "env", pane.env.as_ref());
    set_optional_string(object, "remoteCommand", pane.remote_command.as_deref());
    set_tmux_binding(object, pane.tmux_binding.as_ref());
    object.insert("prompt".to_owned(), Value::String(pane.prompt.clone()));
    object.insert(
        "status".to_owned(),
        Value::String(status_str(pane.status).to_owned()),
    );
    if let Some(latency) = pane.latency_ms {
        object.insert("latencyMs".to_owned(), json!(latency));
    } else {
        object.remove("latencyMs");
    }
    if let Some(scope) = pane.local_machine_scope {
        object.insert(
            "localMachineScope".to_owned(),
            Value::String(local_scope_str(scope).to_owned()),
        );
    } else {
        object.remove("localMachineScope");
    }
    set_optional_string(object, "outputHistory", pane.output_history.as_deref());
    Ok(row)
}

/// 已知 target 在原对象上修改有限字段，保留第三方扩展；Unavailable 原文完全不动。
fn merge_target(existing: Option<&Value>, target: &WorkspaceTargetRef) -> Value {
    if let WorkspaceTargetRef::Unavailable { raw, .. } = target {
        return raw.clone();
    }
    let mut value = existing
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let Some(object) = value.as_object_mut() else {
        return value;
    };
    match target {
        WorkspaceTargetRef::Local { profile_id } => {
            object.insert("kind".to_owned(), Value::String("local".to_owned()));
            set_optional_string(object, "profileId", profile_id.as_deref());
        }
        WorkspaceTargetRef::Ssh { host_id }
        | WorkspaceTargetRef::Telnet { host_id }
        | WorkspaceTargetRef::Serial { host_id } => {
            let kind = match target {
                WorkspaceTargetRef::Ssh { .. } => "ssh",
                WorkspaceTargetRef::Telnet { .. } => "telnet",
                _ => "serial",
            };
            object.insert("kind".to_owned(), Value::String(kind.to_owned()));
            object.insert(
                "hostId".to_owned(),
                Value::String(host_id.as_str().to_owned()),
            );
        }
        WorkspaceTargetRef::DockerContainer {
            host_id,
            container_id,
            runtime,
            container_name,
            user,
            workdir,
        } => {
            object.insert(
                "kind".to_owned(),
                Value::String("dockerContainer".to_owned()),
            );
            object.insert(
                "hostId".to_owned(),
                Value::String(host_id.as_str().to_owned()),
            );
            object.insert(
                "containerId".to_owned(),
                Value::String(container_id.clone()),
            );
            object.insert(
                "runtime".to_owned(),
                Value::String(container_runtime_str(*runtime).to_owned()),
            );
            set_optional_string(object, "containerName", container_name.as_deref());
            set_optional_string(object, "user", user.as_deref());
            set_optional_string(object, "workdir", workdir.as_deref());
        }
        WorkspaceTargetRef::Unavailable { raw, .. } => return raw.clone(),
    }
    value
}

/// 新旧 pane 使用同一完整字段集，lines 留给旧 raw 行，避免把输出历史复制进 UI state。
fn encode_new_pane(pane: &WorkspaceSessionPane) -> Value {
    let mut object = Map::new();
    object.insert("id".to_owned(), Value::String(pane.id.as_str().to_owned()));
    object.insert("title".to_owned(), Value::String(pane.title.clone()));
    object.insert(
        "machineId".to_owned(),
        Value::String(pane.machine_id.clone()),
    );
    object.insert(
        "mode".to_owned(),
        Value::String(pane_mode_str(pane.mode).to_owned()),
    );
    if let Some(target) = &pane.target {
        object.insert("target".to_owned(), merge_target(None, target));
    }
    set_optional_string(
        &mut object,
        "remoteHostId",
        pane.remote_host_id.as_ref().map(HostId::as_str),
    );
    set_optional_string(&mut object, "containerId", pane.container_id.as_deref());
    set_optional_string(&mut object, "profileId", pane.profile_id.as_deref());
    set_optional_string(&mut object, "shell", pane.shell.as_deref());
    set_optional_string_array(&mut object, "args", pane.args.as_deref());
    set_optional_string(&mut object, "cwd", pane.cwd.as_deref());
    set_optional_string(&mut object, "currentCwd", pane.current_cwd.as_deref());
    set_optional_string_map(&mut object, "env", pane.env.as_ref());
    set_optional_string(&mut object, "remoteCommand", pane.remote_command.as_deref());
    set_tmux_binding(&mut object, pane.tmux_binding.as_ref());
    object.insert("prompt".to_owned(), Value::String(pane.prompt.clone()));
    object.insert(
        "status".to_owned(),
        Value::String(status_str(pane.status).to_owned()),
    );
    if let Some(latency) = pane.latency_ms {
        object.insert("latencyMs".to_owned(), json!(latency));
    }
    if let Some(scope) = pane.local_machine_scope {
        object.insert(
            "localMachineScope".to_owned(),
            Value::String(local_scope_str(scope).to_owned()),
        );
    }
    set_optional_string(&mut object, "outputHistory", pane.output_history.as_deref());
    object.insert("lines".to_owned(), Value::Array(Vec::new()));
    Value::Object(object)
}

/// 保存 tmux binding 时合并原对象以保留后续 v3 扩展字段。
fn set_tmux_binding(object: &mut Map<String, Value>, binding: Option<&WorkspaceTmuxBinding>) {
    let Some(binding) = binding else {
        object.remove("tmuxBinding");
        return;
    };
    let mut value = object
        .get("tmuxBinding")
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let Some(fields) = value.as_object_mut() else {
        return;
    };
    fields.insert(
        "targetRef".to_owned(),
        Value::String(binding.target_ref.clone()),
    );
    fields.insert(
        "sessionId".to_owned(),
        Value::String(binding.session_id.clone()),
    );
    fields.insert(
        "sessionName".to_owned(),
        Value::String(binding.session_name.clone()),
    );
    fields.insert(
        "attachedAt".to_owned(),
        Value::String(binding.attached_at.clone()),
    );
    set_optional_string(fields, "socketName", binding.socket_name.as_deref());
    set_optional_string(fields, "socketPath", binding.socket_path.as_deref());
    object.insert("tmuxBinding".to_owned(), value);
}

/// 保存 tab group 时只写冻结版本已知字段，其他颜色字面值原样持有。
fn group_color_str(color: &WorkspaceTabGroupColor) -> &str {
    match color {
        WorkspaceTabGroupColor::Blue => "blue",
        WorkspaceTabGroupColor::Pink => "pink",
        WorkspaceTabGroupColor::Purple => "purple",
        WorkspaceTabGroupColor::Mint => "mint",
        WorkspaceTabGroupColor::Amber => "amber",
        WorkspaceTabGroupColor::Teal => "teal",
        WorkspaceTabGroupColor::Orange => "orange",
        WorkspaceTabGroupColor::Gray => "gray",
        WorkspaceTabGroupColor::Unsupported(value) => value,
    }
}

/// 将稳定 v3 文件 tab 权限写回协议字符串。
fn file_access_str(access: WorkspaceFileAccess) -> &'static str {
    match access {
        WorkspaceFileAccess::Readonly => "readonly",
        WorkspaceFileAccess::Editable => "editable",
    }
}

/// 将 stable source 枚举写回原始 camelCase 命名。
fn file_source_str(source: WorkspaceFileSource) -> &'static str {
    match source {
        WorkspaceFileSource::Sftp => "sftp",
        WorkspaceFileSource::Container => "container",
        WorkspaceFileSource::ComposeYaml => "composeYaml",
        WorkspaceFileSource::Workspace => "workspace",
        WorkspaceFileSource::Local => "local",
    }
}

/// 文件布局方向序列化沿用旧 v3 的小写英文 token。
fn split_direction_str(direction: SplitDirection) -> &'static str {
    match direction {
        SplitDirection::Horizontal => "horizontal",
        SplitDirection::Vertical => "vertical",
    }
}

/// pane mode 写回与冻结 TS 的值完全一致。
fn pane_mode_str(mode: WorkspacePaneMode) -> &'static str {
    match mode {
        WorkspacePaneMode::Local => "local",
        WorkspacePaneMode::Ssh => "ssh",
        WorkspacePaneMode::Telnet => "telnet",
        WorkspacePaneMode::Serial => "serial",
        WorkspacePaneMode::Container => "container",
        WorkspacePaneMode::Preview => "preview",
    }
}

/// status 仅表示保存时状态，Root 恢复后仍以实际 Session 结果覆盖显示。
fn status_str(status: WorkspaceMachineStatus) -> &'static str {
    match status {
        WorkspaceMachineStatus::Online => "online",
        WorkspaceMachineStatus::Offline => "offline",
        WorkspaceMachineStatus::Warning => "warning",
    }
}

/// 本地 shell scope 写回旧 session 的稳定枚举值。
fn local_scope_str(scope: WorkspaceLocalMachineScope) -> &'static str {
    match scope {
        WorkspaceLocalMachineScope::Sidebar => "sidebar",
        WorkspaceLocalMachineScope::Workspace => "workspace",
    }
}

/// container runtime 严格映射回原始 target DTO。
fn container_runtime_str(runtime: WorkspaceContainerRuntime) -> &'static str {
    match runtime {
        WorkspaceContainerRuntime::Docker => "docker",
        WorkspaceContainerRuntime::Podman => "podman",
    }
}

/// 使用同一可选字符串策略删除缺省字段，确保 JSON 不积累空占位。
fn set_optional_string(object: &mut Map<String, Value>, name: &str, value: Option<&str>) {
    if let Some(value) = value {
        object.insert(name.to_owned(), Value::String(value.to_owned()));
    } else {
        object.remove(name);
    }
}

/// argv 以数组写入，不通过 shell join 产生转义或参数边界变化。
fn set_optional_string_array(
    object: &mut Map<String, Value>,
    name: &str,
    values: Option<&[String]>,
) {
    if let Some(values) = values {
        object.insert(
            name.to_owned(),
            Value::Array(values.iter().cloned().map(Value::String).collect()),
        );
    } else {
        object.remove(name);
    }
}

/// 环境变量 map 的顺序不表达含义，序列化仍按稳定 key 顺序输出。
fn set_optional_string_map(
    object: &mut Map<String, Value>,
    name: &str,
    values: Option<&BTreeMap<String, String>>,
) {
    if let Some(values) = values {
        object.insert(
            name.to_owned(),
            Value::Object(
                values
                    .iter()
                    .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                    .collect(),
            ),
        );
    } else {
        object.remove(name);
    }
}

/// 递归拒绝 workspace session 中的凭据正文，同时允许 vault 引用字段。
fn contains_credential_material(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(contains_credential_material),
        Value::Object(fields) => fields.iter().any(|(name, value)| {
            let normalized = name.to_ascii_lowercase().replace('-', "_");
            matches!(
                normalized.as_str(),
                "password"
                    | "credential_secret"
                    | "inline_private_key"
                    | "key_passphrase"
                    | "private_key"
            ) || contains_credential_material(value)
        }),
        _ => false,
    }
}
