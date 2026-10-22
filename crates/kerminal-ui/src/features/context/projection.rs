//! Context inspector 的安全展示类型与 Agent 关联规则。
//!
//! @author kongweiguang

use std::fmt;

use kerminal_application::{
    agents::AgentSessionView,
    sessions::{CommandBlockSnapshot, CommandBlockState, CommandBlockText},
};

use crate::terminal_canvas::{
    TerminalLinkCandidateId, TerminalLinkSnapshot, TerminalLinkSnapshotIdentity, TerminalLinkSource,
};
use kerminal_domain::{
    agents::{AgentSessionStatus, AgentTargetLiveStatus},
    identity::{AgentSessionId, HostId, PaneId, SessionId, TabId},
    sessions::SessionLifecycle,
};

/// Workspace 快照和真实 pane 运行态的合并状态；损坏/缺失不会被解释成空成功。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextProjectionStatus {
    NoWorkspace,
    Restoring,
    Ready,
    Partial,
    Unavailable,
}

/// 连接目标类型是白名单枚举，避免把命令、环境变量或错误正文误当作安全元数据。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextConnectionKind {
    Terminal,
    Local,
    Ssh,
    Telnet,
    Serial,
    Container,
    FileWorkspace,
    Unsupported,
}

/// 目标连接状态只描述资源生命周期，不复述基础设施错误或用户 terminal 输出。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextConnectionStatus {
    Ready,
    Starting,
    Restoring,
    Closing,
    Failed,
    Offline,
}

/// 可由 ToolRail 导航的标签页摘要；不复制文件正文或未知 workspace 原始 JSON。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextTabSummary {
    pub tab_id: TabId,
    pub title: String,
    pub kind: ContextConnectionKind,
    pub machine_id: Option<String>,
    pub active: bool,
}

/// 当前活动对象的目标摘要；不携带命令、认证信息或不可验证的原始配置。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextTargetSummary {
    pub kind: ContextConnectionKind,
    pub label: String,
    pub host_id: Option<HostId>,
    pub container_id: Option<String>,
    pub reference: Option<String>,
}

/// 位置来源和可信度分开表达，避免把保存时的 cwd 冒充当前 shell 的实时目录。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextLocationSource {
    LiveShellIntegration,
    PersistedCurrentDirectory,
    ConfiguredStartDirectory,
    WorkspaceFile,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextLocationConfidence {
    High,
    Medium,
    Low,
    Unknown,
}

/// 位置只显示可信的已知路径；没有实时来源时说明它来自持久状态或启动配置。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextLocationSummary {
    pub path: Option<String>,
    pub source: ContextLocationSource,
    pub confidence: ContextLocationConfidence,
}

/// 焦点 pane 的安全摘要；不含环境变量、命令、终端快照和 transport 凭据。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextPaneSummary {
    pub pane_id: PaneId,
    pub tab_id: TabId,
    pub session_id: Option<SessionId>,
    pub title: String,
    pub kind: ContextConnectionKind,
    pub host_id: Option<HostId>,
    pub target_ref: Option<String>,
    pub status: ContextConnectionStatus,
    pub cwd: Option<String>,
    pub shell: Option<String>,
    pub columns: u16,
    pub rows: u16,
}

/// Workspace 运行态资源数量只来自真实 Root pane/tab 映射，不从快照文本推算。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextRuntimeSummary {
    pub tab_count: usize,
    pub pane_count: usize,
    pub ready_pane_count: usize,
    pub pending_pane_count: usize,
    pub failed_pane_count: usize,
    pub latency_ms: Option<u64>,
    pub tmux_attached: Option<bool>,
}

/// 每个来源是否真实读取由 Root 标注；尚未采集与成功空结果保持不同状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextSourceStatus {
    Available,
    Loading,
    Stale,
    Unavailable,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextSourceSummary {
    pub source: String,
    pub status: ContextSourceStatus,
    pub updated_at: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextDiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// 诊断只由安全分类和固定说明构成，禁止透传基础设施错误或终端正文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextDiagnosticSummary {
    pub id: String,
    pub code: String,
    pub summary: String,
    pub severity: ContextDiagnosticSeverity,
}

/// 发现索引状态必须区分“确实为空”和“来源尚未接入”，避免展示伪成功空列表。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextArtifactIndexState {
    Available,
    Partial,
    Loading,
    Stale,
    NotConnected,
    Unavailable,
}

/// 可安全展示的发现项只保留白名单类别、标题和时间，不保存正文、prompt 或输出片段。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextArtifactSummary {
    pub category: ContextArtifactCategory,
    pub title: String,
    pub observed_at: Option<String>,
}

/// Context 只呈现由可信 shell integration 事件登记的有限发现类型。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextArtifactCategory {
    Link,
    Command,
}

/// Artifact index 的数据和来源状态独立表达；Unavailable 时 entries 必须为空。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextArtifactIndex {
    pub state: ContextArtifactIndexState,
    pub entries: Vec<ContextArtifactSummary>,
}

impl ContextArtifactIndex {
    /// 当前没有真实安全索引 producer 时显式标记未接线，不把它转换为“没有发现项”。
    pub fn not_connected() -> Self {
        Self {
            state: ContextArtifactIndexState::NotConnected,
            entries: Vec::new(),
        }
    }
}

/// Context 可展示的链接只保留安全标签和不可猜 URI 的快照内候选 ID。
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ContextTerminalLinkSummary {
    pub(crate) id: TerminalLinkCandidateId,
    pub(crate) source: TerminalLinkSource,
    pub(crate) label: std::sync::Arc<str>,
}

/// 链接缓存身份与可见状态分开，旧候选可留作禁用展示但不可跨快照打开。
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ContextTerminalLinkIndex {
    pub(crate) state: ContextArtifactIndexState,
    pub(crate) identity: Option<TerminalLinkSnapshotIdentity>,
    pub(crate) entries: Vec<ContextTerminalLinkSummary>,
}

impl fmt::Debug for ContextTerminalLinkIndex {
    /// 快照身份包含会话 owner；诊断仅输出来源状态和条数，不泄露绑定或候选信息。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContextTerminalLinkIndex")
            .field("state", &self.state)
            .field("has_identity", &self.identity.is_some())
            .field("entry_count", &self.entries.len())
            .finish()
    }
}

impl ContextTerminalLinkIndex {
    /// 无实际链接 producer 时显式展示未接入，不能冒充扫描后的空结果。
    pub(crate) fn not_connected() -> Self {
        Self {
            state: ContextArtifactIndexState::NotConnected,
            identity: None,
            entries: Vec::new(),
        }
    }

    /// 当前 pane 已有可读视口、索引尚未提交时显示加载状态。
    pub(crate) fn loading() -> Self {
        Self {
            state: ContextArtifactIndexState::Loading,
            identity: None,
            entries: Vec::new(),
        }
    }

    /// 终端目标尚不可读时明确标记链接索引不可用。
    pub(crate) fn unavailable() -> Self {
        Self {
            state: ContextArtifactIndexState::Unavailable,
            identity: None,
            entries: Vec::new(),
        }
    }

    /// 只映射共享 scanner 已验证候选；URI 留在 Root cache，Context 仅持候选 ID 和安全标签。
    ///
    /// @author kongweiguang
    pub(crate) fn from_snapshot(snapshot: &TerminalLinkSnapshot, is_current: bool) -> Self {
        Self {
            state: terminal_link_state(is_current, snapshot.partial),
            identity: Some(snapshot.identity.clone()),
            entries: snapshot
                .candidates
                .iter()
                .map(|candidate| ContextTerminalLinkSummary {
                    id: candidate.id(),
                    source: candidate.source(),
                    label: std::sync::Arc::from(candidate.label()),
                })
                .collect(),
        }
    }
}

/// 过期状态优先于部分扫描，避免把旧链接误报为当前可点击结果。
///
/// @author kongweiguang
pub(super) fn terminal_link_state(is_current: bool, is_partial: bool) -> ContextArtifactIndexState {
    if !is_current {
        ContextArtifactIndexState::Stale
    } else if is_partial {
        ContextArtifactIndexState::Partial
    } else {
        ContextArtifactIndexState::Available
    }
}

/// 链接只在会话 owner/generation、快照 revision 和 viewport 三者完全一致时仍可打开。
///
/// @author kongweiguang
pub(crate) fn terminal_link_identity_matches(
    identity: &TerminalLinkSnapshotIdentity,
    current_binding: &kerminal_application::sessions::UiSessionBinding,
    current_session_id: &SessionId,
    current_revision: u64,
    current_scrollback_offset: usize,
    current_size: kerminal_domain::sessions::TerminalSize,
) -> bool {
    &identity.binding == current_binding
        && identity.binding.session_id.as_str() == current_session_id.as_str()
        && identity.revision == current_revision
        && identity.scrollback_offset == current_scrollback_offset
        && identity.size == current_size
}

/// OSC 133 快照投影只暴露有界块数、完成状态与退出码，不把可信命令正文显示到 Context。
///
/// @author kongweiguang
pub(super) fn command_block_artifact_index(
    snapshot: &CommandBlockSnapshot,
) -> ContextArtifactIndex {
    let entries = snapshot
        .blocks
        .iter()
        .enumerate()
        .map(|(index, block)| {
            let text_status = match &block.command {
                CommandBlockText::Known(_) => "正文已过滤",
                CommandBlockText::Unknown(_) => "正文不可用",
            };
            let state_status = match block.state {
                CommandBlockState::Running => "运行中".to_owned(),
                CommandBlockState::Finished {
                    exit_code: Some(code),
                } => format!("退出码 {code}"),
                CommandBlockState::Finished { exit_code: None } => "已结束".to_owned(),
                CommandBlockState::Unknown(_) => "结果未知".to_owned(),
            };
            ContextArtifactSummary {
                category: ContextArtifactCategory::Command,
                title: format!("命令块 {} · {text_status} · {state_status}", index + 1),
                observed_at: None,
            }
        })
        .collect();
    ContextArtifactIndex {
        state: ContextArtifactIndexState::Partial,
        entries,
    }
}

/// Root 根据实际 pane/tab/target 投影的只读 workspace 上下文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ContextWorkspaceProjection {
    pub(crate) status: ContextProjectionStatus,
    pub(crate) subject_title: String,
    pub(crate) machine_id: Option<String>,
    pub(crate) selected_host_id: Option<HostId>,
    pub(crate) tabs: Vec<ContextTabSummary>,
    pub(crate) active_tab_id: Option<TabId>,
    pub(crate) active_target: Option<ContextTargetSummary>,
    pub(crate) location: ContextLocationSummary,
    pub(crate) active_pane: Option<ContextPaneSummary>,
    pub(crate) runtime: ContextRuntimeSummary,
    pub(crate) sources: Vec<ContextSourceSummary>,
    pub(crate) diagnostics: Vec<ContextDiagnosticSummary>,
    pub(crate) terminal_links: ContextTerminalLinkIndex,
}

impl ContextWorkspaceProjection {
    /// 未有可读取的 workspace 时显示明确不可用状态，而非构造无差异的成功空快照。
    pub(crate) fn unavailable() -> Self {
        Self {
            status: ContextProjectionStatus::NoWorkspace,
            subject_title: "当前工作区".to_owned(),
            machine_id: None,
            selected_host_id: None,
            tabs: Vec::new(),
            active_tab_id: None,
            active_target: None,
            location: ContextLocationSummary {
                path: None,
                source: ContextLocationSource::Unavailable,
                confidence: ContextLocationConfidence::Unknown,
            },
            active_pane: None,
            runtime: ContextRuntimeSummary::default(),
            sources: vec![ContextSourceSummary {
                source: "工作区".to_owned(),
                status: ContextSourceStatus::Unavailable,
                updated_at: None,
            }],
            diagnostics: Vec::new(),
            terminal_links: ContextTerminalLinkIndex::not_connected(),
        }
    }
}

/// Agent 生命周期只显示安全业务状态，不展示启动命令、launch args 或工作目录。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextAgentLifecycle {
    Running,
    ReadyToResume,
    Exited,
    Stale,
    RecoveryPending,
    Archived,
}

/// 显示 Agent 与当前 pane 的精确绑定关系；StaleTarget 不表示当前 pane 可替代旧目标。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextAgentAssociation {
    Bound,
    StaleTarget,
    ClosedTarget,
}

/// 与当前 workspace 目标关联的 Agent 安全摘要。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextAgentSummary {
    pub agent_session_id: AgentSessionId,
    pub title: String,
    pub provider: String,
    pub lifecycle: ContextAgentLifecycle,
    pub association: ContextAgentAssociation,
    pub updated_at: String,
}

/// 只按当前 pane 的明确 session 或同 pane/tab 的 stale binding 匹配，不猜其它目标。
pub(crate) fn associated_agent_summaries(
    projection: &ContextWorkspaceProjection,
    views: &[AgentSessionView],
) -> Vec<ContextAgentSummary> {
    let Some(pane) = projection.active_pane.as_ref() else {
        return Vec::new();
    };

    let mut matches = views
        .iter()
        .filter(|view| view.session.status != AgentSessionStatus::Archived)
        .filter_map(|view| {
            let binding = view.session.target.as_ref()?;
            let association = match binding.live_status {
                AgentTargetLiveStatus::Ready
                    if pane.session_id.is_some()
                        && binding.terminal_session_id.as_ref() == pane.session_id.as_ref()
                        && binding
                            .pane_id
                            .as_ref()
                            .is_none_or(|id| id == &pane.pane_id)
                        && binding.tab_id.as_ref().is_none_or(|id| id == &pane.tab_id) =>
                {
                    ContextAgentAssociation::Bound
                }
                AgentTargetLiveStatus::Stale
                    if binding.pane_id.as_ref() == Some(&pane.pane_id)
                        && binding.tab_id.as_ref() == Some(&pane.tab_id)
                        && binding
                            .target_kind
                            .as_deref()
                            .is_none_or(|kind| kind == connection_kind_key(pane.kind))
                        && binding.target_ref.as_deref().is_none_or(|target_ref| {
                            pane.target_ref.as_deref() == Some(target_ref)
                        }) =>
                {
                    ContextAgentAssociation::StaleTarget
                }
                AgentTargetLiveStatus::Closed
                    if binding.pane_id.as_ref() == Some(&pane.pane_id)
                        && binding.tab_id.as_ref() == Some(&pane.tab_id) =>
                {
                    ContextAgentAssociation::ClosedTarget
                }
                AgentTargetLiveStatus::Unbound
                | AgentTargetLiveStatus::Ready
                | AgentTargetLiveStatus::Stale
                | AgentTargetLiveStatus::Closed => return None,
            };

            Some(ContextAgentSummary {
                agent_session_id: view.session.agent_session_id.clone(),
                title: safe_context_text(&view.session.title),
                provider: view.session.agent_id.as_str().to_owned(),
                lifecycle: agent_lifecycle(view),
                association,
                updated_at: safe_context_text(&view.session.updated_at),
            })
        })
        .collect::<Vec<_>>();

    matches.sort_by(|left, right| {
        association_rank(right.association)
            .cmp(&association_rank(left.association))
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| left.agent_session_id.cmp(&right.agent_session_id))
    });
    matches
}

/// Agent 标题来自用户可写的会话文件，去除控制字符并限长后再进入原生 UI。
pub(super) fn safe_context_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(256)
        .collect()
}

/// 领域生命周期映射为 inspector 语义，不用 terminal presence 推断恢复是否成功。
pub(super) fn agent_lifecycle(view: &AgentSessionView) -> ContextAgentLifecycle {
    if view.session.status == AgentSessionStatus::Archived {
        return ContextAgentLifecycle::Archived;
    }
    if view.resume_recovery.is_some() {
        return ContextAgentLifecycle::RecoveryPending;
    }
    if view.session.status == AgentSessionStatus::Stale {
        return ContextAgentLifecycle::Stale;
    }
    match view.terminal.as_ref().map(|terminal| terminal.lifecycle) {
        Some(SessionLifecycle::Running | SessionLifecycle::Starting) => {
            ContextAgentLifecycle::Running
        }
        Some(SessionLifecycle::Exited | SessionLifecycle::Closed | SessionLifecycle::Failed)
        | Some(SessionLifecycle::Closing) => ContextAgentLifecycle::Exited,
        None => ContextAgentLifecycle::ReadyToResume,
    }
}

/// 排序优先级只影响摘要显示，不参与 target 选择或 Agent 生命周期变更。
fn association_rank(association: ContextAgentAssociation) -> u8 {
    match association {
        ContextAgentAssociation::Bound => 3,
        ContextAgentAssociation::StaleTarget => 2,
        ContextAgentAssociation::ClosedTarget => 1,
    }
}

/// 比较持久绑定 targetKind 时使用稳定旧 wire 标签，不序列化领域枚举。
fn connection_kind_key(kind: ContextConnectionKind) -> &'static str {
    match kind {
        ContextConnectionKind::Terminal => "terminal",
        ContextConnectionKind::Local => "local",
        ContextConnectionKind::Ssh => "ssh",
        ContextConnectionKind::Telnet => "telnet",
        ContextConnectionKind::Serial => "serial",
        ContextConnectionKind::Container => "container",
        ContextConnectionKind::FileWorkspace => "fileWorkspace",
        ContextConnectionKind::Unsupported => "unsupported",
    }
}

/// 点击后由 Root 导航稳定 workspace identity，不通过 panel 调用连接或目标绑定用例。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextNavigationTarget {
    Host(HostId),
    Tab(TabId),
    Pane(PaneId),
    Agent(AgentSessionId),
}
