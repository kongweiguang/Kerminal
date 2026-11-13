//! 运维工作台的状态边界与跨组件事件契约。
//!
//! @author kongweiguang

mod actions;
mod compose_yaml;
mod confirmations;
mod container_delete;
mod container_mutation;
mod container_panel;
mod forms;
pub mod proxy_panel;
mod quickref;
mod render;
#[cfg(feature = "right-rail-acceptance")]
mod right_rail_acceptance;
#[cfg(feature = "right-rail-acceptance")]
mod right_rail_acceptance_containers;
#[cfg(feature = "right-rail-acceptance")]
mod right_rail_acceptance_forwarding;
#[cfg(feature = "right-rail-acceptance")]
mod right_rail_acceptance_lifecycle;
mod state;

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use gpui_kit::{Context, Entity, EventEmitter, Pixels, Point, ScrollHandle};
use kerminal_application::{
    ApplicationTaskExecutor,
    operations::{OperationsService, PortForwardDefinition, TmuxCapability},
    preferences::ThemeMode,
    sessions::UiSessionBinding,
};
use kerminal_domain::{
    files::FileEndpoint,
    identity::{HostId, PaneId},
    operations::{
        ComposeAction, ComposeProjectTarget, ContainerInspect, ContainerRuntime, ContainerStats,
        ContainerSummary, ContainerTarget, ForwardKind, ServerInfoSnapshot, ServerInfoTarget,
        TmuxPane, TmuxPaneCapture, TmuxSession, TmuxTarget, TmuxWindow,
    },
};
use tokio_util::sync::CancellationToken;

use crate::ui_motion::{TargetDataEntrance, motion_allowed};

use self::{
    forms::OperationsInputs,
    state::{
        DirtyOperationTargets, LatestRequestState, SectionPhase, SectionSelection, SectionState,
        ServerInfoHistory, ServerInfoPanel, TmuxMutationFeedback, TmuxMutationIdentity,
        defer_tmux_feedback, take_deferred_tmux_feedback,
    },
};

pub(crate) use compose_yaml::COMPOSE_YAML_PREVIEW_MAX_BYTES;
use container_panel::ContainerPanelActionTicket;
pub use container_panel::{
    ContainerPanelContext, ContainerPanelUnavailableReason, ContainerReadKind,
};
pub use state::ContainerMutationRequest;

/// 固定项必须等同宿主列表确认目标，失败与仍在读取分开返回给侧栏请求。
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SidebarContainerReveal {
    Waiting,
    Opened,
    Unavailable,
}

/// Root 对当前 pane context 的结论；Unavailable 与 Absent 必须区分，前者禁止选主机回退。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ActiveOperationsPaneTarget {
    Absent,
    Unavailable,
    Local,
    Ssh(HostId),
}

/// 只在没有活动 pane context 时采用侧栏明确选择；已有 pane 无论本机、未就绪或身份不符都优先拒绝回退。
///
/// @author kongweiguang
pub(crate) fn resolve_tool_rail_operation_target(
    pane_target: ActiveOperationsPaneTarget,
    selected_host_id: Option<HostId>,
) -> ToolRailOperationTarget {
    match pane_target {
        ActiveOperationsPaneTarget::Absent => selected_host_id
            .map_or(ToolRailOperationTarget::Unavailable, |host_id| {
                ToolRailOperationTarget::Ssh { host_id }
            }),
        ActiveOperationsPaneTarget::Unavailable => ToolRailOperationTarget::Unavailable,
        ActiveOperationsPaneTarget::Local => ToolRailOperationTarget::Local,
        ActiveOperationsPaneTarget::Ssh(host_id) => ToolRailOperationTarget::Ssh { host_id },
    }
}

/// 固定右栏目标将无目标、本机 pane 和 SSH HostId 明确区分；None 不再隐式代表本机。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRailOperationTarget {
    Unavailable,
    Local,
    Ssh { host_id: HostId },
}

/// 添加转发只从显式 SSH 目标取主机；完整运维页才使用用户选中的主机，不把 Local/Unavailable 回退为 SSH。
///
/// @author kongweiguang
pub(crate) fn resolve_forward_add_host(
    tool_rail_target: Option<&ToolRailOperationTarget>,
    selected_host_id: Option<&HostId>,
) -> Option<HostId> {
    match tool_rail_target {
        Some(ToolRailOperationTarget::Ssh { host_id }) => Some(host_id.clone()),
        Some(ToolRailOperationTarget::Unavailable | ToolRailOperationTarget::Local) => None,
        None => selected_host_id.cloned(),
    }
}

/// 容器详情只为 Ports 提供说明上下文；即使解析器给出宿主，也不能把容器会话当作 SSH 隧道目标。
///
/// @author kongweiguang
pub(crate) fn resolve_ports_panel_target(
    target: ToolRailOperationTarget,
    container_target: Option<&ContainerTarget>,
) -> ToolRailOperationTarget {
    if container_target.is_some() {
        ToolRailOperationTarget::Unavailable
    } else {
        target
    }
}

/// 无可用 SSH HostId 时按是否处于容器目标显示 main 对应的提示，不改变转发提交门禁。
///
/// @author kongweiguang
pub(crate) fn forward_unavailable_hint(
    container_target: Option<&ContainerTarget>,
) -> (&'static str, &'static str) {
    if container_target.is_some() {
        (
            "当前容器目标不支持直接创建 SSH 隧道。请切回宿主 SSH 主机管理端口转发。",
            "The current container target cannot create SSH tunnels directly. Switch back to the host SSH target to manage port forwarding.",
        )
    } else {
        ("请选择 SSH 主机。", "Select an SSH host.")
    }
}

/// 编辑始终取原定义主机，新建只取打开表单时的快照；刻意不接收易变的当前面板目标。
///
/// @author kongweiguang
pub(crate) fn resolve_forward_form_host(
    edit_host_id: Option<&HostId>,
    add_host_snapshot: Option<&HostId>,
) -> Option<HostId> {
    edit_host_id.or(add_host_snapshot).cloned()
}

/// 只有仍打开且绑定明确 SSH 主机的表单可提交，隐藏或失去快照都必须阻止写入。
///
/// @author kongweiguang
pub(crate) fn forward_form_can_save(form_open: bool, host_id: Option<&HostId>) -> bool {
    form_open && host_id.is_some()
}

impl ToolRailOperationTarget {
    /// 服务输入只承载 SSH 主机；Unavailable 与 Local 都不冒充另一台远端主机。
    ///
    /// @author kongweiguang
    fn host_id(&self) -> Option<HostId> {
        match self {
            Self::Ssh { host_id } => Some(host_id.clone()),
            Self::Unavailable | Self::Local => None,
        }
    }
}

/// 面板只发出需要工作区或文件服务完成的请求；Root 负责分配 Tab/Pane 所有权。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationsEvent {
    /// 根模态协调器重新投影唯一运维确认状态，不复制目标或业务前置条件。
    RootConfirmationChanged,
    /// 隧道失败交给工作区短暂提示，避免在每个行或标题重复保留结果。
    ForwardError { message: String },
    OpenContainerFilesRequested {
        container_panel_context: Option<ContainerPanelContext>,
        target: ContainerTarget,
        path: String,
    },
    ContainerExecRequested {
        container_panel_context: Option<ContainerPanelContext>,
        target: ContainerTarget,
        command: Vec<String>,
    },
    ContainerLogsRequested {
        container_panel_context: Option<ContainerPanelContext>,
        target: ContainerTarget,
        name: String,
    },
    ContainerPinRequested {
        context: Option<ContainerPanelContext>,
        target: ContainerTarget,
        container: Box<ContainerSummary>,
    },
    OpenComposeConfigRequested {
        container_panel_context: Option<ContainerPanelContext>,
        endpoint: FileEndpoint,
        selection_name: String,
        root_path: Option<String>,
    },
    /// 页内 YAML 预览复用文件用例；request_id 只用于回执隔离，不授予新的目标权限。
    ComposeYamlPreviewRequested {
        context: Option<ContainerPanelContext>,
        request_id: u64,
        endpoint: FileEndpoint,
    },
    ContainerMutationRequested {
        context: ContainerPanelContext,
        request: ContainerMutationRequest,
    },
    ComposeMutationRequested {
        context: ContainerPanelContext,
        target: ComposeProjectTarget,
        action: ComposeAction,
    },
    ContainerReadRequested {
        context: ContainerPanelContext,
        target: ContainerTarget,
        kind: ContainerReadKind,
    },
    ContainerRefreshRequested {
        context: ContainerPanelContext,
        runtime: ContainerRuntime,
    },
    SendTmuxQuickrefRequested {
        pane_id: PaneId,
        binding: UiSessionBinding,
        target: TmuxTarget,
        session_id: String,
        text: String,
        kind: TmuxQuickrefKind,
    },
    TmuxAttachRequested {
        target: TmuxTarget,
        session_id: String,
        session_name: String,
        terminal: Option<TmuxAttachTerminalBinding>,
    },
    /// 请求 Root 向点击时冻结的原终端队列发送一次默认分离快捷键。
    TmuxDetachCurrentRequested {
        target: TmuxTarget,
        terminal: TmuxAttachTerminalBinding,
    },
}

/// Root 路由区分 tmux 快捷键和 shell 命令，避免把控制字节当普通文本处理。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TmuxQuickrefKind {
    Command,
    Shortcut,
}

/// Root 确认可见、聚焦且 Ready 的 tmux pane；运维面板只携带投影，不自行解析当前终端。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxQuickrefTerminalBinding {
    pub pane_id: PaneId,
    pub binding: UiSessionBinding,
}

/// Root 投影的当前本机或 SSH 终端目标；互斥变体避免 profile 与 host 混成含糊目标。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TmuxAttachTarget {
    Local { profile_id: Option<String> },
    Ssh { host_id: HostId },
}

impl TmuxAttachTarget {
    /// 将可信 pane 目标转换成运维服务输入；本机 profile 只沿用实际绑定值，不推断默认 profile。
    ///
    /// @author kongweiguang
    pub fn to_tmux_target(&self) -> TmuxTarget {
        match self {
            Self::Local { profile_id } => TmuxTarget {
                profile_id: profile_id.clone(),
                ..TmuxTarget::default()
            },
            Self::Ssh { host_id } => TmuxTarget {
                host_id: Some(host_id.clone()),
                ..TmuxTarget::default()
            },
        }
    }

    /// 返回与侧栏目标对齐所需的 host；本机必须保持 `None`，不能借用已选中的远端主机。
    ///
    /// @author kongweiguang
    pub fn host_id(&self) -> Option<&HostId> {
        match self {
            Self::Local { .. } => None,
            Self::Ssh { host_id } => Some(host_id),
        }
    }

    /// 比较 pane 的 host/profile 身份而保留调用方 socket 作用域，避免 attach 被重定向到别的 pane。
    ///
    /// @author kongweiguang
    pub fn matches_tmux_target(&self, target: &TmuxTarget) -> bool {
        let bound = self.to_tmux_target();
        target.validate().is_ok()
            && target.host_id == bound.host_id
            && target.profile_id == bound.profile_id
    }
}

/// 主机级 tmux 查询不授予终端写入；attach 控件必须持有精确 pane target binding。
///
/// @author kongweiguang
pub(super) fn tmux_attach_binding_matches_target(
    binding_target: Option<&TmuxAttachTarget>,
    target: &TmuxTarget,
) -> bool {
    binding_target.is_some_and(|binding| binding.matches_tmux_target(target))
}

/// Root 投影的当前终端身份，供 tmux attach 精确写回原 pane 而不新建 Tab。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxAttachTerminalBinding {
    pub pane_id: PaneId,
    pub target: TmuxAttachTarget,
    pub binding: UiSessionBinding,
    pub working_directory: Option<String>,
}

/// 固定 tmux 的 attach owner 与 host/profile 读取目标必须分开判断。
///
/// 同 host 换 pane 只改变 owner：必须递增视图代次并撤销冻结的确认、detach 意图和
/// 迟到写回可见性，但会话列表及窗口/pane/capture 详情仍绑定同一个 `TmuxTarget`，
/// 可以直接复用。只有目标身份变化或目标失效才重置读取与详情，避免旧目标数据或
/// 旧请求跨目标显示；重置后是否立即读取仍由可见性和初始化状态决定。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TmuxAttachTransition {
    pub(super) owner_changed: bool,
    pub(super) reset_reads: bool,
    pub(super) load_sessions: bool,
}

/// 把 owner 变化与数据目标变化拆开，避免换 pane 时清空同一 tmux 服务器的会话与详情。
///
/// @author kongweiguang
fn plan_tmux_attach_transition(
    current: Option<&TmuxAttachTerminalBinding>,
    next: Option<&TmuxAttachTerminalBinding>,
    previous_target: &TmuxTarget,
    next_target: &TmuxTarget,
    target_available: bool,
    phase: SectionPhase,
    can_read: bool,
) -> TmuxAttachTransition {
    let owner_changed = match (current, next) {
        (Some(current), Some(next)) => {
            current.pane_id != next.pane_id
                || current.target != next.target
                || current.binding != next.binding
        }
        (None, None) => false,
        _ => true,
    };
    let target_changed = previous_target != next_target;
    let reset_reads = target_changed || !target_available;
    // 首次挂载或从不可用恢复时，只有可见且已初始化才主动补读；隐藏面板等待激活后读取。
    let load_sessions =
        target_available && can_read && (reset_reads || phase == SectionPhase::NotLoaded);
    TmuxAttachTransition {
        owner_changed,
        reset_reads,
        load_sessions,
    }
}

/// 分离请求冻结 tmux 查询目标和可见终端身份，不要求该 pane 曾提交 attach 命令。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
struct TmuxDetachIntent {
    target: TmuxTarget,
    terminal: TmuxAttachTerminalBinding,
}

/// detach 只绑定 pane、transport 和完整 UI session binding；工作目录不是写入身份。
///
/// @author kongweiguang
fn tmux_detach_terminal_matches(
    expected: &TmuxAttachTerminalBinding,
    current: &TmuxAttachTerminalBinding,
) -> bool {
    expected.pane_id == current.pane_id
        && expected.target == current.target
        && expected.binding == current.binding
}

/// 分离请求必须仍命中 tmux 服务目标及完整可见终端代次。
/// 工作目录变化不改变写入目标；pane、transport、owner 或 generation 任一变化都拒绝。
///
/// @author kongweiguang
fn tmux_detach_intent_matches_current(
    intent: &TmuxDetachIntent,
    target: &TmuxTarget,
    terminal: &TmuxAttachTerminalBinding,
) -> bool {
    &intent.target == target
        && tmux_detach_terminal_matches(&intent.terminal, terminal)
        && terminal.target.matches_tmux_target(target)
}

/// 只有目标与 pane 仍精确匹配且没有重复请求或并行写操作时才允许发送分离键。
///
/// @author kongweiguang
fn tmux_detach_intent_can_send(
    intent: &TmuxDetachIntent,
    target: &TmuxTarget,
    terminal: &TmuxAttachTerminalBinding,
    detach_request_pending: bool,
    busy: bool,
) -> bool {
    !detach_request_pending && !busy && tmux_detach_intent_matches_current(intent, target, terminal)
}

impl EventEmitter<OperationsEvent> for OperationsView {}

/// 运维页的真实能力分区；切换只改变当前视图，不重建服务、连接或在途请求。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationsSection {
    ServerInfo,
    Containers,
    Tmux,
    PortForwards,
}

/// 服务器资源的自动刷新周期；手动模式只保留首次加载和用户主动刷新。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerInfoRefreshInterval {
    Manual,
    OneSecond,
    ThreeSeconds,
    FiveSeconds,
    TenSeconds,
    ThirtySeconds,
    SixtySeconds,
    FiveMinutes,
}

impl ServerInfoRefreshInterval {
    /// 提供有限的产品周期选项，避免任意值造成远端探针压力。
    pub(super) const OPTIONS: [Self; 8] = [
        Self::Manual,
        Self::OneSecond,
        Self::ThreeSeconds,
        Self::FiveSeconds,
        Self::TenSeconds,
        Self::ThirtySeconds,
        Self::SixtySeconds,
        Self::FiveMinutes,
    ];

    /// 固定数字和单位的紧凑写法，避免语言切换改变右栏选择器宽度；手动项仍按语言本地化。
    pub(super) const fn label(self) -> (&'static str, &'static str) {
        match self {
            Self::Manual => ("手动", "Manual"),
            Self::OneSecond => ("1s", "1s"),
            Self::ThreeSeconds => ("3s", "3s"),
            Self::FiveSeconds => ("5s", "5s"),
            Self::TenSeconds => ("10s", "10s"),
            Self::ThirtySeconds => ("30s", "30s"),
            Self::SixtySeconds => ("60s", "60s"),
            Self::FiveMinutes => ("5min", "5min"),
        }
    }

    /// 隐藏时停掉周期采样，让既有 driver 等待可见性唤醒；重显沿负责即时刷新。
    ///
    /// @author kongweiguang
    pub(super) fn period(self, panel_active: bool) -> Option<Duration> {
        if !panel_active {
            return None;
        }
        let configured = match self {
            Self::Manual => return None,
            Self::OneSecond => Duration::from_secs(1),
            Self::ThreeSeconds => Duration::from_secs(3),
            Self::FiveSeconds => Duration::from_secs(5),
            Self::TenSeconds => Duration::from_secs(10),
            Self::ThirtySeconds => Duration::from_secs(30),
            Self::SixtySeconds => Duration::from_secs(60),
            Self::FiveMinutes => Duration::from_secs(300),
        };
        Some(configured)
    }
}

/// 自动采样 driver 的最小设置快照；设置变更唤醒同一个串行任务。
#[derive(Clone)]
struct ServerMonitorConfiguration {
    revision: u64,
    server_generation: u64,
    immediate_revision: u64,
    panel_active: bool,
    server_section_visible: bool,
    interval: ServerInfoRefreshInterval,
    wake: CancellationToken,
}

/// 详情读取状态与结果快照分开，刷新或取消时仍能展示同目标的最后成功结果。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum DetailReadPhase {
    #[default]
    Idle,
    Loading,
    Ready,
    Cancelled,
    Failed,
}

/// 详情投影按精确目标保留快照；目标变化才丢弃旧数据，失败与取消不伪装成空结果。
///
/// @author kongweiguang
pub(super) struct DetailReadState<Target, Value> {
    target: Option<Target>,
    data: Option<Value>,
    phase: DetailReadPhase,
    feedback: Option<String>,
}

impl<Target, Value> Default for DetailReadState<Target, Value> {
    /// 空详情从未读取状态开始，避免要求领域投影实现默认值。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            target: None,
            data: None,
            phase: DetailReadPhase::Idle,
            feedback: None,
        }
    }
}

impl<Target: PartialEq, Value> DetailReadState<Target, Value> {
    /// 新目标才清空投影；同目标刷新保留可识别的旧快照直到新回执到达。
    ///
    /// @author kongweiguang
    pub(super) fn begin(&mut self, target: Target) {
        if self.target.as_ref() != Some(&target) {
            self.data = None;
        }
        self.target = Some(target);
        self.phase = DetailReadPhase::Loading;
        self.feedback = None;
    }

    /// 只接受当前目标且仍处于读取中的回执，错误会保留该目标最后一次成功结果。
    ///
    /// @author kongweiguang
    pub(super) fn complete(&mut self, target: &Target, result: Result<Value, String>) -> bool {
        if self.target.as_ref() != Some(target) || self.phase != DetailReadPhase::Loading {
            return false;
        }
        match result {
            Ok(data) => {
                self.data = Some(data);
                self.phase = DetailReadPhase::Ready;
                self.feedback = None;
            }
            Err(message) => {
                self.phase = DetailReadPhase::Failed;
                self.feedback = Some(message);
            }
        }
        true
    }

    /// 仅在真正读取中标记取消，保留快照和精确目标供用户显式重试。
    ///
    /// @author kongweiguang
    pub(super) fn cancel(&mut self, message: String) -> bool {
        if self.phase != DetailReadPhase::Loading {
            return false;
        }
        self.phase = DetailReadPhase::Cancelled;
        self.feedback = Some(message);
        true
    }

    /// 目标失效或 runtime 改变时丢弃所有详情，避免对失效对象展示旧数据。
    ///
    /// @author kongweiguang
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    /// 查询状态供 UI 判断是否仍在等待；读取投影不会暴露可变字段。
    ///
    /// @author kongweiguang
    pub(super) fn phase(&self) -> DetailReadPhase {
        self.phase
    }

    /// 只读访问最后成功快照，避免渲染或用例绕过结果状态写数据。
    ///
    /// @author kongweiguang
    pub(super) fn data(&self) -> Option<&Value> {
        self.data.as_ref()
    }

    /// 以同一详情区域呈现取消或失败原因，并保持领域错误的展示边界。
    ///
    /// @author kongweiguang
    pub(super) fn feedback(&self) -> Option<&str> {
        self.feedback.as_deref()
    }

    /// 读取目标供权威列表校验，只有目标仍存在时才保留可重试详情。
    ///
    /// @author kongweiguang
    pub(super) fn target(&self) -> Option<&Target> {
        self.target.as_ref()
    }

    /// 区分尚未请求与加载中但还没有成功数据的状态。
    ///
    /// @author kongweiguang
    pub(super) fn has_state(&self) -> bool {
        self.target.is_some()
    }
}

/// 同一个容器的日志和资源快照属于不同读取目标，不能交叉复用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ContainerOutputTarget {
    pub(super) target: ContainerTarget,
    pub(super) kind: ContainerOutputKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ContainerOutputKind {
    Logs,
    Stats,
}

/// 保留资源快照的结构和原始数据，展示层不能从本地化摘要反向解析指标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ContainerOutputData {
    Logs(String),
    Stats(ContainerStats),
}

impl ContainerOutputData {
    /// 完整运维工作台沿用原摘要，右栏监控则从同一快照绘制 main 的指标分区。
    pub(super) fn display_text(&self) -> String {
        match self {
            Self::Logs(output) => output.clone(),
            Self::Stats(stats) => format!(
                "CPU {} · {} {} / {} · {} {} · {} {} · PIDs {}",
                stats.cpu_percent.as_deref().unwrap_or("—"),
                crate::locale::text("内存", "memory"),
                stats.memory_usage.as_deref().unwrap_or("—"),
                stats.memory_percent.as_deref().unwrap_or("—"),
                crate::locale::text("网络", "network"),
                stats.network_io.as_deref().unwrap_or("—"),
                crate::locale::text("磁盘", "disk"),
                stats.block_io.as_deref().unwrap_or("—"),
                stats.pids.as_deref().unwrap_or("—"),
            ),
        }
    }

    /// 只允许资源读取分支取得结构化数据，日志不能冒充空指标快照。
    pub(super) fn stats(&self) -> Option<&ContainerStats> {
        match self {
            Self::Stats(stats) => Some(stats),
            Self::Logs(_) => None,
        }
    }
}

/// tmux 内容需要区分“服务器不可用”和成功查询到零个 session。
#[derive(Clone, Debug)]
pub(super) struct TmuxSnapshot {
    pub(super) capability: TmuxCapability,
    pub(super) sessions: Vec<TmuxSession>,
}

/// 选定 session 的窗口、pane 与捕获结果只属于该 session 的展示投影。
#[derive(Default)]
pub(super) struct TmuxDetailState {
    pub(super) session_id: Option<String>,
    pub(super) session_name: Option<String>,
    pub(super) windows: Option<Vec<TmuxWindow>>,
    pub(super) windows_loading: bool,
    pub(super) windows_error: Option<String>,
    pub(super) panes: Option<Vec<TmuxPane>>,
    pub(super) panes_loading: bool,
    pub(super) panes_error: Option<String>,
    pub(super) capture: Option<TmuxPaneCapture>,
    pub(super) capture_loading: bool,
    pub(super) capture_error: Option<String>,
    pub(super) pane_target_window_id: Option<String>,
    pub(super) capture_target_pane_id: Option<String>,
    pub(super) generation: u64,
    pub(super) cancellation: Option<CancellationToken>,
    capture_request: LatestRequestState,
}

impl TmuxDetailState {
    /// 面板隐藏时只结束进行中的 tmux 详情读取，并为对应操作留下明确重试提示。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_pending_reads(&mut self) {
        let mut list_read_cancelled = false;
        if self.windows_loading || self.panes_loading {
            if let Some(cancellation) = self.cancellation.take() {
                cancellation.cancel();
            }
            let message = crate::locale::text(
                "读取已取消，可再次点击当前 session 的“窗口 / 分屏”重试。",
                "Read cancelled; click this session's “Windows / panes” action to retry.",
            )
            .to_string();
            if self.windows_loading {
                self.windows_loading = false;
                self.windows_error = Some(message.clone());
            }
            if self.panes_loading {
                self.panes_loading = false;
                self.panes_error = Some(
                    crate::locale::text(
                        "分屏读取已取消，可再次点击当前窗口重试。",
                        "Pane list cancelled; click the current window to retry.",
                    )
                    .to_string(),
                );
            }
            self.generation = self.generation.wrapping_add(1).max(1);
            list_read_cancelled = true;
        }
        if self.capture_loading {
            self.capture_request.cancel_pending();
            self.capture_loading = false;
            self.capture_error = Some(
                crate::locale::text(
                    "输出捕获已取消，可再次点击该 pane 的“捕获 200 行”重试。",
                    "Capture cancelled; click this pane's “Capture 200 lines” action to retry.",
                )
                .to_string(),
            );
        }
        if list_read_cancelled {
            self.cancellation = None;
        }
    }
}

/// 每项能力各自保留最后成功快照和错误状态，Docker/tmux 故障不会遮蔽服务器与转发。
pub struct OperationsView {
    operations: Arc<OperationsService>,
    executor: ApplicationTaskExecutor,
    host_target: Option<HostId>,
    /// None 仅代表完整运维页；固定右栏用 Unavailable 显式阻止本机回退。
    tool_rail_target: Option<ToolRailOperationTarget>,
    /// Ports 只用精确容器身份区分 main 的提示；它不会授权该容器的 SSH 宿主创建隧道。
    tool_rail_container_target: Option<ContainerTarget>,
    /// 仅用于标题和转发行文案；实际目标仍由 HostId/typed target 独立绑定。
    host_display_names: HashMap<HostId, gpui_kit::SharedString>,
    host_subtitles: HashMap<HostId, gpui_kit::SharedString>,
    /// ToolRail 隐藏时保留低频服务器采样，但停止高频查询和转发状态订阅。
    active: bool,
    runtime: ContainerRuntime,
    /// 容器 runtime 与 host 一起构成容器列表的请求世代，runtime 切换不作废其他区块。
    container_generation: u64,
    target_generation: u64,
    /// tmux 视图世代覆盖同 host pane 切换，迟到 mutation 回执不能改写新焦点的 UI。
    tmux_view_generation: u64,
    /// 服务器信息面板当前明确查询的主机或容器，不从选中 Tab 隐式推断。
    server_info_target: ServerInfoTarget,
    server_generation: u64,
    server: SectionState<ServerInfoSnapshot>,
    server_info_panel: ServerInfoPanel,
    server_history: ServerInfoHistory,
    server_network_filter: render::server::ServerNetworkFilter,
    server_process_sort: render::server::ServerProcessSort,
    server_refresh_interval: ServerInfoRefreshInterval,
    server_monitor_revision: u64,
    server_monitor_immediate_revision: u64,
    server_monitor_started: bool,
    server_monitor_cancellation: CancellationToken,
    server_monitor_wake: CancellationToken,
    containers: SectionState<Vec<ContainerSummary>>,
    /// 列表导航和筛选保留在宿主视图，跨 pane 不重建同宿主的用户选择。
    container_host: render::container_panel::ContainerHostPresentation,
    /// 离开容器列表前的滚动位置；返回列表时恢复，避免长列表跳回顶部。
    container_list_scroll: Option<Point<Pixels>>,
    /// 离开 Compose 项目页前的滚动位置；容器详情或暂时不可用返回项目时恢复。
    container_project_scroll: Option<Point<Pixels>>,
    /// 运维正文的唯一滚动句柄；页面或目标切换时显式归零或恢复记录位置。
    operations_scroll: ScrollHandle,
    /// 页内预览固定请求端点，避免跨项目或宿主的迟到内容覆盖当前 YAML。
    compose_yaml: compose_yaml::ComposeYamlPreviewState,
    /// 保留完整 Compose 执行范围，拆除后仍显示再次启动入口，不依据同名容器补猜路径。
    compose_projects: Vec<ComposeProjectTarget>,
    /// 拆除确认固定原 host/runtime/目录/配置序列，切目标时撤销确认。
    compose_pending_down: Option<ComposeProjectTarget>,
    /// 根层确认同时冻结当前容器 context；最终写入仍需 Root 对 live context 再做一次门禁。
    compose_pending_context: Option<ContainerPanelContext>,
    /// Down 最终提交只消费确认回调登记的同目标一次性票据。
    confirmed_compose_down: Option<(ContainerPanelContext, ComposeProjectTarget)>,
    /// 命令回执与同宿主列表刷新分开记录，未核对前禁止重复写入。
    container_mutations: container_mutation::ContainerMutationTracker,
    /// 容器操作的确定结果与错误分开呈现，避免成功提示误用危险色。
    container_success_notice: Option<String>,
    /// 确认内容拥有独立焦点句柄，根层负责保存及恢复原工作区焦点。
    confirmation_focus: Option<gpui_kit::FocusHandle>,
    tmux: SectionState<TmuxSnapshot>,
    forwards: SectionState<Vec<kerminal_application::operations::ManagedPortForwardSnapshot>>,
    /// 未显示目标的写操作回执要求返回时重新读取权威状态。
    dirty_targets: DirtyOperationTargets,
    /// 四个固定数据区各自只保留一次“目标变化后首个成功结果”的局部入场状态；
    /// 不复制列表内容，也不参与数据展示与请求生命周期。
    server_data_entrance: TargetDataEntrance,
    container_data_entrance: TargetDataEntrance,
    tmux_data_entrance: TargetDataEntrance,
    forward_data_entrance: TargetDataEntrance,
    /// Revision watcher 随视图取消；等待与读取仍由应用执行器追踪。
    forward_watch_cancellation: CancellationToken,
    forward_watch_started: bool,
    forward_watch_generation: u64,
    busy: bool,
    /// 写操作使用独立取消令牌；只有拿到真实回执才可报告成功或失败。
    mutation_cancellation: Option<CancellationToken>,
    status: Option<String>,
    /// 隐藏期间已提交写入的最新回执按 Host/runtime 暂存，回来时反馈并回读，不触碰当前其它目标。
    deferred_container_feedback: VecDeque<(Option<HostId>, ContainerRuntime, String)>,
    /// 离开原 tmux 目标后才到达的写回执按完整目标和操作身份有界暂存；返回该目标时
    /// 消费一次，当前同目标换 pane 直接显示，不把失败、取消或未知结果静默丢弃。
    deferred_tmux_feedback: VecDeque<TmuxMutationFeedback>,
    /// 暂存队列因同身份覆盖或容量淘汰而丢弃的未展示结果条数；丢弃必须显式可见，
    /// 否则用户无法察觉有结果被有界策略吞掉。
    deferred_tmux_feedback_losses: state::TmuxFeedbackLosses,
    container_output: DetailReadState<ContainerOutputTarget, ContainerOutputData>,
    container_inspect: DetailReadState<ContainerTarget, ContainerInspect>,
    /// None 表示传统运维页；固定容器工具必须持有 Root 每轮重投影的精确上下文。
    container_panel_context: Option<ContainerPanelContext>,
    /// 只读详情页的最后选中区块；内容按需读取，不通过后台轮询更新。
    container_info_tab: ContainerReadKind,
    container_inspect_request: LatestRequestState,
    container_output_request: LatestRequestState,
    container_search: String,
    container_delete_confirmation: Option<container_delete::ContainerDeleteConfirmation>,
    container_output_target_label: Option<String>,
    tmux_details: TmuxDetailState,
    /// 展开与快捷命令是呈现状态，不改变 tmux 服务的选中对象或连接身份。
    tmux_panel: render::tmux::TmuxPanelPresentation,
    tmux_current_session_id: Option<String>,
    tmux_current_session_name: Option<String>,
    /// 快捷内容仅能发到 Root 明确投影的当前 tmux pane，不从选中主机推断写入目标。
    tmux_quickref_binding: Option<TmuxQuickrefTerminalBinding>,
    /// tmux attach 只写入 Root 显式投影的当前同目标本机/SSH pane。
    tmux_attach_binding: Option<TmuxAttachTerminalBinding>,
    /// Root 正校验并排队分离按键时挡住重复点击；失败回执解除 pending 后可重试。
    tmux_detach_pending: Option<TmuxDetachIntent>,
    tmux_pending_kill: Option<String>,
    tmux_rename_target: Option<String>,
    /// 弹窗固定 host/profile/socket 与视图代次，按键和按钮不能转向后来切换的终端。
    tmux_dialog_context: Option<(TmuxTarget, u64)>,
    /// 外层 Option 标记创建弹框已捕获目录；内层 None 表示打开时没有可继承 cwd。
    tmux_create_cwd_snapshot: Option<Option<String>>,
    /// 改名与结束确认保留当时的会话身份；刷新不能把旧 ID 解释成后来创建的会话。
    tmux_dialog_session: Option<TmuxSession>,
    tmux_quickref_open: bool,
    tmux_create_form_open: bool,
    forward_kind: ForwardKind,
    /// 编辑保留原定义作为 CAS 基线；表单不提供主机改绑操作。
    forward_edit_expected: Option<PortForwardDefinition>,
    /// 临时运行实例编辑后成为可管理的新定义，原 ID 只用于精确停止旧实例。
    forward_edit_transient_id: Option<String>,
    /// Add 表单打开时冻结 SSH 主机，目标栏后续变化不得改变本次定义的归属。
    forward_add_host_expected: Option<HostId>,
    /// 未知保存保留原事务 ID 与期望内容，核对前不允许再次写入。
    forward_save_pending: Option<PortForwardDefinition>,
    /// 删除确认冻结定义和目标代次，过期确认不能删除后来切换或修改的对象。
    forward_pending_delete: Option<(PortForwardDefinition, u64)>,
    theme_mode: ThemeMode,
    compact_layout: bool,
    section_selection: SectionSelection,
    initialized: bool,
    inputs: Option<OperationsInputs>,
    /// SOCKS 应用面板只读取运行态快照，具体 session binding 由 Root 注入。
    proxy_panel: Option<Entity<proxy_panel::ProxyApplicationPanel>>,
    proxy_panel_observer: Option<gpui_kit::Subscription>,
}

impl OperationsView {
    /// 只注入组合根创建的用例和任务执行器；输入实体延迟到真实窗口出现后初始化。
    ///
    /// @author kongweiguang
    pub fn new(operations: Arc<OperationsService>, executor: ApplicationTaskExecutor) -> Self {
        Self {
            operations,
            executor,
            host_target: None,
            tool_rail_target: None,
            tool_rail_container_target: None,
            host_display_names: HashMap::new(),
            host_subtitles: HashMap::new(),
            active: true,
            runtime: ContainerRuntime::Docker,
            container_generation: 1,
            target_generation: 1,
            tmux_view_generation: 1,
            server_info_target: ServerInfoTarget::Local,
            server_generation: 1,
            server: SectionState::new(),
            server_info_panel: ServerInfoPanel::default(),
            server_history: ServerInfoHistory::default(),
            server_network_filter: Default::default(),
            server_process_sort: Default::default(),
            server_refresh_interval: ServerInfoRefreshInterval::ThreeSeconds,
            server_monitor_revision: 1,
            server_monitor_immediate_revision: 1,
            server_monitor_started: false,
            server_monitor_cancellation: CancellationToken::new(),
            server_monitor_wake: CancellationToken::new(),
            containers: SectionState::new(),
            container_host: Default::default(),
            container_list_scroll: None,
            container_project_scroll: None,
            operations_scroll: ScrollHandle::new(),
            compose_yaml: Default::default(),
            compose_projects: Vec::new(),
            compose_pending_down: None,
            compose_pending_context: None,
            confirmed_compose_down: None,
            container_mutations: Default::default(),
            container_success_notice: None,
            confirmation_focus: None,
            tmux: SectionState::new(),
            forwards: SectionState::new(),
            dirty_targets: DirtyOperationTargets::default(),
            server_data_entrance: TargetDataEntrance::default(),
            container_data_entrance: TargetDataEntrance::default(),
            tmux_data_entrance: TargetDataEntrance::default(),
            forward_data_entrance: TargetDataEntrance::default(),
            forward_watch_cancellation: CancellationToken::new(),
            forward_watch_started: false,
            forward_watch_generation: 0,
            busy: false,
            mutation_cancellation: None,
            status: None,
            deferred_container_feedback: VecDeque::new(),
            deferred_tmux_feedback: VecDeque::new(),
            deferred_tmux_feedback_losses: 0,
            container_output: DetailReadState::default(),
            container_inspect: DetailReadState::default(),
            container_panel_context: None,
            container_info_tab: ContainerReadKind::Inspect,
            container_inspect_request: LatestRequestState::default(),
            container_output_request: LatestRequestState::default(),
            container_search: String::new(),
            container_delete_confirmation: None,
            container_output_target_label: None,
            tmux_details: TmuxDetailState::default(),
            tmux_panel: Default::default(),
            tmux_current_session_id: None,
            tmux_current_session_name: None,
            tmux_quickref_binding: None,
            tmux_attach_binding: None,
            tmux_detach_pending: None,
            tmux_pending_kill: None,
            tmux_rename_target: None,
            tmux_dialog_context: None,
            tmux_create_cwd_snapshot: None,
            tmux_dialog_session: None,
            tmux_quickref_open: false,
            tmux_create_form_open: false,
            forward_kind: ForwardKind::Local,
            forward_edit_expected: None,
            forward_edit_transient_id: None,
            forward_add_host_expected: None,
            forward_save_pending: None,
            forward_pending_delete: None,
            theme_mode: ThemeMode::System,
            compact_layout: false,
            section_selection: SectionSelection::dashboard(),
            initialized: false,
            inputs: None,
            proxy_panel: None,
            proxy_panel_observer: None,
        }
    }

    /// 只在 Root 已把 attach 命令与回车排进精确终端 FIFO 后登记瞬时意图；
    /// 固定栏以终端回显反馈受理，不插入成功提示挤动正文，也不把排队当作连接完成。
    ///
    /// @author kongweiguang
    pub(crate) fn report_tmux_attach_queued(
        &mut self,
        target: &TmuxTarget,
        session_id: &str,
        session_name: &str,
        terminal: &TmuxAttachTerminalBinding,
        cx: &mut Context<Self>,
    ) {
        let current_target = self.tmux_target();
        let current_context = target == &current_target
            && self.tmux_attach_binding.as_ref() == Some(terminal)
            && self.tmux_details.session_id.as_deref() == Some(session_id)
            && !session_id.trim().is_empty()
            && terminal.target.matches_tmux_target(target);
        if current_context && self.is_fixed_tmux_view() {
            self.status = None;
        } else if current_context {
            self.status = Some(
                crate::locale::text(
                    format!("已将 tmux 会话“{session_name}”的连接命令提交到当前终端；请核对回显后刷新会话，确认连接状态。"),
                    format!(
                        "Queued the attach command for tmux session “{session_name}”; verify its terminal output, then refresh sessions to confirm the connection."
                    ),
                )
                .to_string(),
            );
        }
        cx.notify();
    }

    /// Header 只依赖当前服务目标和 Root 投影的终端身份，不要求 tmux session 已 attached。
    ///
    /// @author kongweiguang
    fn current_tmux_attach_intent(&self) -> Option<TmuxDetachIntent> {
        if !self.tmux_target_available() || self.busy {
            return None;
        }
        let target = self.tmux_target();
        let terminal = self.tmux_attach_binding.as_ref()?;
        let intent = TmuxDetachIntent {
            target,
            terminal: terminal.clone(),
        };
        tmux_detach_intent_can_send(
            &intent,
            &intent.target,
            terminal,
            self.tmux_detach_pending.is_some(),
            self.busy,
        )
        .then_some(intent)
    }

    /// 分离事件先登记单次 pending 以挡住双击；Root 再按冻结 owner 核验并报告是否成功入队。
    ///
    /// @author kongweiguang
    fn request_tmux_detach_current(&mut self, cx: &mut Context<Self>) {
        let Some(intent) = self.current_tmux_attach_intent() else {
            return;
        };
        self.tmux_detach_pending = Some(intent.clone());
        cx.emit(OperationsEvent::TmuxDetachCurrentRequested {
            target: intent.target,
            terminal: intent.terminal,
        });
        cx.notify();
    }

    /// Root 发键前复核 pending intent、当前目标及原始 pane 代次，拒绝延迟事件改投新目标。
    ///
    /// @author kongweiguang
    pub(crate) fn tmux_detach_request_is_current(
        &self,
        target: &TmuxTarget,
        terminal: &TmuxAttachTerminalBinding,
    ) -> bool {
        let Some(pending) = self.tmux_detach_pending.as_ref() else {
            return false;
        };
        let current = TmuxDetachIntent {
            target: target.clone(),
            terminal: terminal.clone(),
        };
        if !self.tmux_target_available()
            || self.busy
            || target != &self.tmux_target()
            || !self
                .tmux_attach_binding
                .as_ref()
                .is_some_and(|current| tmux_detach_terminal_matches(terminal, current))
            || pending != &current
            || !tmux_detach_intent_matches_current(pending, target, terminal)
        {
            return false;
        }
        true
    }

    /// 固定栏受理后不插入已发送提示；失败仍展示原因，受理不冒充服务端已分离。
    ///
    /// @author kongweiguang
    pub(crate) fn report_tmux_detach_current_result(
        &mut self,
        target: &TmuxTarget,
        terminal: &TmuxAttachTerminalBinding,
        queued: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.tmux_detach_pending.clone() else {
            return;
        };
        if !tmux_detach_intent_matches_current(&pending, target, terminal) {
            return;
        }
        let current_context = target == &self.tmux_target()
            && self
                .tmux_attach_binding
                .as_ref()
                .is_some_and(|current| tmux_detach_terminal_matches(terminal, current));
        self.tmux_detach_pending = None;
        if !current_context {
            cx.notify();
            return;
        }
        self.status = if queued && self.is_fixed_tmux_view() {
            None
        } else {
            Some(if queued {
                crate::locale::text(
                "已发送默认 tmux 分离快捷键 Ctrl+B,d；核对回显后请刷新会话。自定义 prefix 请在快捷键参考中查看。",
                "Sent the default tmux detach shortcut Ctrl+B,d; verify the terminal output, then refresh sessions. For a custom prefix, use the shortcut reference.",
            )
            .to_string()
            } else {
                crate::locale::text(
                "未能向当前终端发送 tmux 分离快捷键；检查目标绑定后重试。",
                "Could not send the tmux detach shortcut to the current terminal; check the target binding and retry.",
            )
            .to_string()
            })
        };
        cx.notify();
    }

    /// 语言变化时刷新已创建表单的提示，不初始化隐藏面板或替换任何输入状态。
    ///
    /// @author kongweiguang
    pub fn refresh_locale(&mut self, window: &mut gpui_kit::Window, cx: &mut Context<Self>) {
        if let Some(inputs) = self.inputs.as_ref() {
            inputs.refresh_locale(window, cx);
        }
    }

    /// 创建独立固定分区视图供 ToolRail 多槽挂载；实例状态独立而服务与执行器由组合根共享。
    pub fn new_panel(
        operations: Arc<OperationsService>,
        executor: ApplicationTaskExecutor,
        section: OperationsSection,
    ) -> Self {
        let mut view = Self::new(operations, executor);
        view.section_selection = SectionSelection::fixed(section);
        view.active = false;
        view
    }

    /// 预置容器面板目标而不启动 I/O；首次读取由面板激活后的生命周期触发。
    ///
    /// @author kongweiguang
    pub fn with_initial_container_panel_context(mut self, context: ContainerPanelContext) -> Self {
        self.apply_container_panel_context(context.clone());
        self.container_panel_context = Some(context);
        self
    }

    /// 更新 Root 校验出的终端或文件宿主上下文；身份变化会撤销旧确认，已提交写入仍等待原回执。
    ///
    /// @author kongweiguang
    pub fn set_container_panel_context(
        &mut self,
        context: ContainerPanelContext,
        cx: &mut Context<Self>,
    ) {
        if self.container_panel_context.as_ref() == Some(&context) {
            return;
        }
        let previous_container_scope = (self.host_target.clone(), self.runtime);
        // 不可用分支只切换面板门禁并保留同宿主快照，恢复时不应把保留内容当成新目标重播；
        // 若恢复后确实换了宿主，reset_for_target 会清空旧数据，保守跳过也比误播安全。
        let previous_was_unavailable = matches!(
            self.container_panel_context,
            Some(ContainerPanelContext::Unavailable { .. })
        );
        if matches!(context, ContainerPanelContext::Unavailable { .. }) {
            // 连接中、终端结束或协议不支持只是面板暂时不可用；保留同宿主已建立的导航与快照，
            // 回到可用 pane 时再恢复原页，避免一次聚焦丢失项目/容器层级。
            let was_unavailable = matches!(
                self.container_panel_context,
                Some(ContainerPanelContext::Unavailable { .. })
            );
            if !was_unavailable && self.active {
                self.leave_container_page_scroll();
            } else {
                self.scroll_operations_to_top();
            }
            self.container_panel_context = Some(context.clone());
            self.apply_container_panel_context(context);
            self.container_delete_confirmation = None;
            self.compose_pending_down = None;
            self.compose_pending_context = None;
            self.confirmed_compose_down = None;
            cx.emit(OperationsEvent::RootConfirmationChanged);
            self.arm_container_data_entrance_if_changed(&previous_container_scope, cx);
            cx.notify();
            return;
        }
        let same_host = self
            .container_panel_context
            .as_ref()
            .is_some_and(|previous| container_panel::same_host_context_scope(previous, &context));
        if same_host {
            // 同宿主跨 pane 或文件 Tab 保留读取；未提交确认仍固定原身份，切换后必须重新发起。
            self.container_panel_context = Some(context);
            self.container_delete_confirmation = None;
            self.compose_pending_down = None;
            self.compose_pending_context = None;
            self.confirmed_compose_down = None;
            cx.emit(OperationsEvent::RootConfirmationChanged);
            cx.notify();
            return;
        }
        let preserve_container_snapshot =
            self.container_panel_context
                .as_ref()
                .is_some_and(|current| {
                    container_panel::same_container_info_identity(current, &context)
                });
        if preserve_container_snapshot {
            self.container_panel_context = Some(context);
            cx.notify();
            return;
        }

        self.container_panel_context = Some(context.clone());
        self.apply_container_panel_context(context.clone());
        let scope_changed = self
            .container_host
            .reset_for_target(self.host_target.as_ref(), self.runtime);
        self.target_generation = self.target_generation.wrapping_add(1).max(1);
        self.container_generation = self.container_generation.wrapping_add(1).max(1);
        self.container_inspect_request.cancel_pending();
        self.container_output_request.cancel_pending();
        if scope_changed {
            self.compose_yaml.invalidate();
            self.containers.reset(self.container_generation);
            self.container_inspect.reset();
            self.container_output.reset();
            self.container_output_target_label = None;
            self.compose_projects.clear();
            self.reset_operations_scroll();
        } else if matches!(
            self.container_host.page(),
            render::container_panel::ContainerHostPage::List
        ) {
            // 从暂时不可用状态回到同一宿主时恢复列表原位置；项目页恢复项目位置。
            self.container_project_scroll = None;
            self.restore_container_list_scroll();
        } else if matches!(
            self.container_host.page(),
            render::container_panel::ContainerHostPage::ComposeProject { .. }
        ) {
            self.restore_container_project_scroll();
        } else {
            self.scroll_operations_to_top();
        }
        if !scope_changed {
            self.cancel_interrupted_container_reads();
        }
        self.compose_pending_down = None;
        self.compose_pending_context = None;
        self.confirmed_compose_down = None;
        self.container_delete_confirmation = None;
        self.status = None;
        self.container_success_notice = None;
        self.restore_deferred_container_feedback(&context);
        if !previous_was_unavailable {
            self.arm_container_data_entrance_if_changed(&previous_container_scope, cx);
        }
        cx.emit(OperationsEvent::RootConfirmationChanged);
        if self.active && self.initialized {
            self.reload_container_context(cx);
        }
        cx.notify();
    }

    /// 只读访问 Root 最新提供的上下文，供组合根在提交外部动作前作精确比较。
    ///
    /// @author kongweiguang
    pub fn container_panel_context(&self) -> Option<&ContainerPanelContext> {
        self.container_panel_context.as_ref()
    }

    /// 离开容器页前按列表/项目分别记录位置；进入项目或容器页时正文从顶部开始。
    fn leave_container_page_scroll(&mut self) {
        match self.container_host.page() {
            render::container_panel::ContainerHostPage::List => {
                self.container_list_scroll = Some(self.operations_scroll.offset());
                self.container_project_scroll = None;
            }
            render::container_panel::ContainerHostPage::ComposeProject { .. } => {
                self.container_project_scroll = Some(self.operations_scroll.offset());
            }
            render::container_panel::ContainerHostPage::Container { .. } => {}
        }
        self.operations_scroll.set_offset(Default::default());
    }

    /// 返回宿主容器列表时恢复离开前的位置；列表变短时由 GPUI 夹取到有效范围。
    fn restore_container_list_scroll(&mut self) {
        let offset = self.container_list_scroll.take().unwrap_or_default();
        self.operations_scroll.set_offset(offset);
    }

    /// 容器详情返回来源项目时恢复项目容器页签的滚动位置。
    fn restore_container_project_scroll(&mut self) {
        let offset = self.container_project_scroll.take().unwrap_or_default();
        self.operations_scroll.set_offset(offset);
    }

    /// 项目页切页或离开项目时丢弃旧位置，避免把另一页/另一项目的偏移带回来。
    fn clear_container_project_scroll(&mut self) {
        self.container_project_scroll = None;
    }

    /// 返回宿主容器列表并恢复离开前的位置。
    fn back_to_container_list(&mut self) {
        self.container_host.navigate_to_list();
        self.container_project_scroll = None;
        self.restore_container_list_scroll();
    }

    /// 页内切页或切区块时从顶部开始，避免继承上一页的滚动偏移。
    fn scroll_operations_to_top(&mut self) {
        self.operations_scroll.set_offset(Default::default());
    }

    /// 宿主或 runtime 变化时丢弃旧列表/项目位置，避免把位置带到另一个目标。
    fn reset_operations_scroll(&mut self) {
        self.container_list_scroll = None;
        self.container_project_scroll = None;
        self.operations_scroll.set_offset(Default::default());
    }

    /// 回到同一宿主但读取票据已失效时解除 Loading，保留最后快照并重新启用刷新。
    fn cancel_interrupted_container_reads(&mut self) {
        if self.container_inspect.phase() == DetailReadPhase::Loading {
            self.container_inspect.cancel(
                crate::locale::text(
                    "容器详情读取已中断，请刷新。",
                    "The container details read was interrupted; refresh to retry.",
                )
                .to_string(),
            );
        }
        if self.container_output.phase() == DetailReadPhase::Loading {
            self.container_output.cancel(
                crate::locale::text(
                    "容器监控读取已中断，请刷新。",
                    "The container stats read was interrupted; refresh to retry.",
                )
                .to_string(),
            );
        }
        self.compose_yaml.cancel_pending_preview();
    }

    /// 侧栏固定项只在当前宿主、运行时和最新列表同时匹配后进入详情，复用现有 live-context 读取门禁。
    ///
    /// @author kongweiguang
    pub(crate) fn reveal_sidebar_container(
        &mut self,
        expected: &ContainerPanelContext,
        target: &ContainerTarget,
        cx: &mut Context<Self>,
    ) -> SidebarContainerReveal {
        if !self.active || self.container_panel_context.as_ref() != Some(expected) {
            return SidebarContainerReveal::Waiting;
        }
        let host_matches = match expected {
            ContainerPanelContext::Host { host_id, .. } => host_id == &target.host_id,
            ContainerPanelContext::WorkspaceHost { host_id, .. } => {
                target.host_id.as_ref() == Some(host_id)
            }
            _ => false,
        };
        if !host_matches || target.host_id.is_none() {
            return SidebarContainerReveal::Unavailable;
        }
        if self.runtime != target.runtime {
            self.select_runtime(target.runtime, cx);
            return SidebarContainerReveal::Waiting;
        }
        if self.container_host.set_include_stopped(true) {
            self.load_containers(cx);
            return SidebarContainerReveal::Waiting;
        }
        match self.containers.phase() {
            SectionPhase::NotLoaded | SectionPhase::Loading => SidebarContainerReveal::Waiting,
            SectionPhase::Failed | SectionPhase::Cancelled => SidebarContainerReveal::Unavailable,
            SectionPhase::Ready => {
                if !self
                    .containers
                    .data()
                    .is_some_and(|rows| rows.iter().any(|row| row.target == *target))
                {
                    return SidebarContainerReveal::Unavailable;
                }
                self.leave_container_page_scroll();
                self.container_host.select_container_for_inspection(
                    target.clone(),
                    Some(render::container_panel::HostContainerTab::Details),
                );
                self.container_info_tab = ContainerReadKind::Inspect;
                let ticket = self.container_action_ticket(Some(target.clone()));
                self.request_container_read(
                    target.clone(),
                    ContainerReadKind::Inspect,
                    &ticket,
                    cx,
                );
                cx.notify();
                SidebarContainerReveal::Opened
            }
        }
    }

    /// 将不再可见目标的最新回执有界保存；相同 Host/runtime 的旧反馈由新回执覆盖。
    ///
    /// @author kongweiguang
    pub(super) fn remember_container_feedback(
        &mut self,
        host_id: Option<HostId>,
        runtime: ContainerRuntime,
        message: String,
    ) {
        self.deferred_container_feedback
            .retain(|(stored_host, stored_runtime, _)| {
                stored_host != &host_id || *stored_runtime != runtime
            });
        self.deferred_container_feedback
            .push_back((host_id, runtime, message));
        while self.deferred_container_feedback.len() > 8 {
            self.deferred_container_feedback.pop_front();
        }
    }

    /// 只有返回宿主管理模式才消费 Host/runtime 回执，避免串到同宿主的另一容器详情页。
    ///
    /// @author kongweiguang
    fn restore_deferred_container_feedback(&mut self, context: &ContainerPanelContext) {
        let host_id = match context {
            ContainerPanelContext::Host { host_id, .. } => host_id.clone(),
            ContainerPanelContext::WorkspaceHost { host_id, .. } => Some(host_id.clone()),
            _ => return,
        };
        let runtime = self.runtime;
        if let Some(index) =
            self.deferred_container_feedback
                .iter()
                .rposition(|(stored_host, stored_runtime, _)| {
                    stored_host == &host_id && *stored_runtime == runtime
                })
            && let Some((_, _, message)) = self.deferred_container_feedback.remove(index)
        {
            self.status = Some(message);
        }
    }

    /// 返回当前目标时消费该目标的待投递写回执；拼接后清空，避免返回时重复播放，
    /// 也避免在尚未回到该目标时把旧提示串到其它 profile/socket。
    /// 隐藏面板不消费：隐藏期 status 可能随目标重置被清空，留给重新显示时再显示一次。
    /// 丢弃计数同时清零并转为一次显式提示：丢失只报一次，不跨消费重复播放，
    /// 计数不与具体目标绑定，因此提示也不伪投给某个目标的结果区。
    ///
    /// @author kongweiguang
    fn restore_deferred_tmux_feedback(&mut self) {
        if !self.active {
            return;
        }
        let target = self.tmux_target();
        let mut messages = take_deferred_tmux_feedback(&mut self.deferred_tmux_feedback, &target)
            .into_iter()
            .collect::<Vec<_>>();
        let losses = std::mem::take(&mut self.deferred_tmux_feedback_losses);
        // 目标回执与丢失提示同一次消费拼接：丢失只报一次，不跨消费重复播放；
        // 只报条数与原因，不携带目标归属，不把被丢结果伪投成当前目标的内容。
        if losses > 0 {
            messages.push(state::tmux_feedback_loss_notice(losses));
        }
        let Some(message) = messages
            .into_iter()
            .reduce(|joined, message| format!("{joined}\n{message}"))
        else {
            return;
        };
        self.status = Some(match self.status.take() {
            Some(previous) if !previous.is_empty() => format!("{previous}\n{message}"),
            _ => message,
        });
    }

    /// 紧凑布局下确认成功没有文案，但旧代码在代次门禁内会显式清空 status；
    /// 反馈改走投递通道后，None 仍必须保留“清除残留等待/取消提示”的语义，
    /// 否则“已请求取消”会在远端实际成功后永久残留，与真实终态矛盾。
    /// None 只在当前可见目标仍与冻结目标一致时清 status：目标已离开时 status 属于
    /// 新目标，不能代清；隐藏面板的 status 本就会随目标重置被清理，无需处理。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn apply_tmux_mutation_receipt(
        &mut self,
        frozen_target: &TmuxTarget,
        identity: TmuxMutationIdentity,
        message: Option<String>,
    ) {
        match message {
            Some(message) => self.apply_tmux_mutation_feedback(frozen_target, identity, message),
            None => {
                if self.active && &self.tmux_target() == frozen_target {
                    self.status = None;
                }
            }
        }
    }

    /// 写回执只写入仍展示同一完整目标的可见面板；目标已离开、面板隐藏或该目标不再是
    /// 当前查询时，按完整目标与操作身份有界暂存，重新显示且目标仍一致或返回该目标后
    /// 显示一次。当前同目标换 pane 只是视图代次变化，提示仍立即写入，不因代次抑制而丢失。
    /// 暂存侧溢出或同身份覆盖由共享计数记录，待面板下次消费时显式报告，不静默丢弃。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn apply_tmux_mutation_feedback(
        &mut self,
        frozen_target: &TmuxTarget,
        identity: TmuxMutationIdentity,
        message: String,
    ) {
        // 隐藏面板的 status 会在后续目标重置中被清空；先暂存，重显时再消费一次，
        // 避免用户看不到的提示被当成已展示而静默丢弃。
        if self.active && &self.tmux_target() == frozen_target {
            self.status = Some(message);
            return;
        }
        defer_tmux_feedback(
            &mut self.deferred_tmux_feedback,
            &mut self.deferred_tmux_feedback_losses,
            TmuxMutationFeedback {
                target: frozen_target.clone(),
                identity,
                message,
            },
        );
    }

    /// 让 Root 拒绝面板隐藏后迟到的 exec、文件与 Compose 配置事件。
    ///
    /// @author kongweiguang
    pub(crate) fn container_panel_is_active(&self) -> bool {
        self.active
    }

    /// 冻结容器行创建时的 target/context/runtime 代次，旧 UI listener 不能绑定新会话。
    ///
    /// @author kongweiguang
    fn container_action_ticket(
        &self,
        target: Option<ContainerTarget>,
    ) -> ContainerPanelActionTicket {
        ContainerPanelActionTicket {
            context: self.container_panel_context.clone(),
            runtime: self.runtime,
            generation: self.container_generation,
            target,
        }
    }

    /// 事件触发前再次核对票据；Host 受当前 runtime 限制，ContainerInfo 仅可读唯一绑定目标。
    ///
    /// @author kongweiguang
    fn container_action_ticket_is_current(
        &self,
        ticket: &ContainerPanelActionTicket,
        target: Option<&ContainerTarget>,
    ) -> bool {
        if !self.active
            || self.container_generation != ticket.generation
            || self.runtime != ticket.runtime
            || self.container_panel_context != ticket.context
            || ticket.target.as_ref() != target
        {
            return false;
        }
        match (self.container_panel_context.as_ref(), target) {
            (Some(ContainerPanelContext::Unavailable { .. }), _) => false,
            (Some(context @ ContainerPanelContext::Host { host_id, .. }), Some(target)) => {
                container_panel::host_context_matches_target(
                    context,
                    host_id.as_ref(),
                    self.runtime,
                    target,
                )
            }
            (
                Some(context @ ContainerPanelContext::WorkspaceHost { host_id, .. }),
                Some(target),
            ) => container_panel::host_context_matches_target(
                context,
                Some(host_id),
                self.runtime,
                target,
            ),
            (Some(ContainerPanelContext::ContainerInfo { target: bound, .. }), Some(target)) => {
                bound == target
            }
            (Some(ContainerPanelContext::Host { .. }), None) => true,
            (Some(ContainerPanelContext::WorkspaceHost { .. }), None) => true,
            (Some(ContainerPanelContext::ContainerInfo { .. }), None) => false,
            (None, Some(target)) => {
                self.host_target == target.host_id && self.runtime == target.runtime
            }
            (None, None) => !self.section_selection.is_fixed(),
        }
    }

    /// Compose YAML 预览是只读结果，可跨同宿主 Tab 变化继续回填，写操作仍使用精确票据。
    ///
    /// @author kongweiguang
    fn container_read_ticket_is_current(&self, ticket: &ContainerPanelActionTicket) -> bool {
        self.active
            && self.container_generation == ticket.generation
            && self.runtime == ticket.runtime
            && ticket.target.is_none()
            && container_panel::same_read_context_scope(
                self.container_panel_context.as_ref(),
                ticket.context.as_ref(),
            )
    }

    /// 专用面板的手动刷新经 Root live gate；无绑定的旧 dashboard 仍走自己的显式目标读取。
    ///
    /// @author kongweiguang
    fn request_container_panel_refresh(
        &mut self,
        runtime: ContainerRuntime,
        ticket: &ContainerPanelActionTicket,
        cx: &mut Context<Self>,
    ) {
        if !self.container_action_ticket_is_current(ticket, None) {
            return;
        }
        match self.container_panel_context.clone() {
            Some(context @ ContainerPanelContext::Host { .. }) => {
                cx.emit(OperationsEvent::ContainerRefreshRequested { context, runtime });
            }
            Some(context @ ContainerPanelContext::WorkspaceHost { .. }) => {
                cx.emit(OperationsEvent::ContainerRefreshRequested { context, runtime });
            }
            Some(ContainerPanelContext::ContainerInfo { target, .. }) => {
                if runtime == target.runtime
                    && let Some(context) = ticket.context.clone()
                {
                    cx.emit(OperationsEvent::ContainerReadRequested {
                        context,
                        target,
                        kind: self.container_info_tab,
                    });
                }
            }
            Some(ContainerPanelContext::Unavailable { .. }) => {}
            None => {
                if self.runtime != runtime {
                    self.select_runtime(runtime, cx);
                } else {
                    self.load_containers(cx);
                }
            }
        }
    }

    /// 只允许 Host 管理当前 host/runtime 的容器；ContainerInfo 的所有写入都在视图层拒绝。
    ///
    /// @author kongweiguang
    fn container_mutation_allowed(&self, target: &ContainerTarget) -> bool {
        if self.containers.phase() != SectionPhase::Ready
            || !self
                .containers
                .data()
                .is_some_and(|rows| rows.iter().any(|row| row.target == *target))
            || self
                .container_mutations
                .blocks_scope(&target.host_id, target.runtime)
        {
            return false;
        }
        match self.container_panel_context.as_ref() {
            Some(context @ ContainerPanelContext::Host { host_id, .. }) => {
                container_panel::host_context_matches_target(
                    context,
                    host_id.as_ref(),
                    self.runtime,
                    target,
                )
            }
            Some(context @ ContainerPanelContext::WorkspaceHost { host_id, .. }) => {
                container_panel::host_context_matches_target(
                    context,
                    Some(host_id),
                    self.runtime,
                    target,
                )
            }
            Some(ContainerPanelContext::Unavailable { .. })
            | Some(ContainerPanelContext::ContainerInfo { .. }) => false,
            None => {
                !self.section_selection.is_fixed()
                    && self.host_target == target.host_id
                    && self.runtime == target.runtime
            }
        }
    }

    /// 写入按完整 Compose 身份复核已知项目；同名或陈旧菜单不能提交到另一个目录。
    ///
    /// @author kongweiguang
    fn compose_mutation_allowed(&self, target: &ComposeProjectTarget) -> bool {
        self.active
            && self.containers.phase() == SectionPhase::Ready
            && self.host_target == target.host_id
            && self.runtime == target.runtime
            && self
                .compose_projects
                .iter()
                .any(|project| project == target)
            && !self
                .container_mutations
                .blocks_scope(&target.host_id, target.runtime)
            && matches!(
                self.container_panel_context,
                None | Some(ContainerPanelContext::Host { .. })
                    | Some(ContainerPanelContext::WorkspaceHost { .. })
            )
    }

    /// 当前宿主还有未核对写入时，仅保留刷新入口以恢复可操作状态。
    ///
    /// @author kongweiguang
    fn container_write_scope_blocked(&self) -> bool {
        self.container_mutations
            .blocks_scope(&self.host_target, self.runtime)
    }

    /// Host 行读取须命中当前列表范围；ContainerInfo 只能读取上下文唯一指定的容器。
    ///
    /// @author kongweiguang
    fn container_read_allowed(&self, target: &ContainerTarget) -> bool {
        match self.container_panel_context.as_ref() {
            Some(context @ ContainerPanelContext::Host { host_id, .. }) => {
                container_panel::host_context_matches_target(
                    context,
                    host_id.as_ref(),
                    self.runtime,
                    target,
                ) && self
                    .containers
                    .data()
                    .is_some_and(|rows| rows.iter().any(|row| row.target == *target))
            }
            Some(context @ ContainerPanelContext::WorkspaceHost { host_id, .. }) => {
                container_panel::host_context_matches_target(
                    context,
                    Some(host_id),
                    self.runtime,
                    target,
                ) && self
                    .containers
                    .data()
                    .is_some_and(|rows| rows.iter().any(|row| row.target == *target))
            }
            Some(ContainerPanelContext::ContainerInfo { target: bound, .. }) => bound == target,
            Some(ContainerPanelContext::Unavailable { .. }) => false,
            None => {
                !self.section_selection.is_fixed()
                    && self.host_target == target.host_id
                    && self.runtime == target.runtime
                    && self
                        .containers
                        .data()
                        .is_some_and(|rows| rows.iter().any(|row| row.target == *target))
            }
        }
    }

    /// Root 完成 live session 检查后，View 再核对冻结 Host context 才提交生命周期写入。
    ///
    /// @author kongweiguang
    pub(crate) fn submit_container_mutation_for_context(
        &mut self,
        context: &ContainerPanelContext,
        request: ContainerMutationRequest,
        cx: &mut Context<Self>,
    ) {
        if !self.active
            || self.container_panel_context.as_ref() != Some(context)
            || !self.container_mutation_allowed(&request.target)
        {
            return;
        }
        self.mutate_container(request, cx);
    }

    /// Root 完成 live session 检查和 Down 确认后，View 仅接受同 context 的 Compose 写入。
    ///
    /// @author kongweiguang
    pub(crate) fn submit_compose_mutation_for_context(
        &mut self,
        context: &ContainerPanelContext,
        target: ComposeProjectTarget,
        action: ComposeAction,
        cx: &mut Context<Self>,
    ) {
        let confirmed_down = action != ComposeAction::Down
            || self.confirmed_compose_down.take().is_some_and(
                |(confirmed_context, confirmed_target)| {
                    confirmed_context == *context && confirmed_target == target
                },
            );
        if !confirmed_down
            || !self.active
            || self.container_panel_context.as_ref() != Some(context)
            || !matches!(
                context,
                ContainerPanelContext::Host { .. } | ContainerPanelContext::WorkspaceHost { .. }
            )
            || self.host_target != target.host_id
            || self.runtime != target.runtime
            || !self.compose_mutation_allowed(&target)
        {
            if self.active && self.container_panel_context.as_ref() == Some(context) {
                self.status = Some(
                    crate::locale::text(
                        "Compose 目标或列表状态已变化，请刷新后重新选择。",
                        "The Compose target or list changed. Refresh and select it again.",
                    )
                    .to_string(),
                );
                cx.notify();
            }
            return;
        }
        self.execute_compose(target, action, cx);
    }

    /// Root 对详情读取执行 live-context gate 后，View 仍核对目标及可见状态再调用服务。
    ///
    /// @author kongweiguang
    pub(crate) fn submit_container_read_for_context(
        &mut self,
        context: &ContainerPanelContext,
        target: ContainerTarget,
        kind: ContainerReadKind,
        cx: &mut Context<Self>,
    ) {
        if !self.active
            || self.container_panel_context.as_ref() != Some(context)
            || !self.container_read_allowed(&target)
        {
            return;
        }
        self.submit_container_read(target, kind, cx);
    }

    /// Root live gate 通过后才切换运行时并读取宿主清单，旧面板事件无法重启隐藏查询。
    ///
    /// @author kongweiguang
    pub(crate) fn submit_container_refresh_for_context(
        &mut self,
        context: &ContainerPanelContext,
        runtime: ContainerRuntime,
        cx: &mut Context<Self>,
    ) {
        let host_id = match self.container_panel_context.as_ref() {
            Some(ContainerPanelContext::Host { host_id, .. }) => host_id.as_ref(),
            Some(ContainerPanelContext::WorkspaceHost { host_id, .. }) => Some(host_id),
            _ => return,
        };
        if !self.active
            || self.container_panel_context.as_ref() != Some(context)
            || host_id != self.host_target.as_ref()
        {
            return;
        }
        if self.runtime != runtime {
            let previous_container_scope = (self.host_target.clone(), self.runtime);
            self.runtime = runtime;
            self.container_host
                .reset_for_target(self.host_target.as_ref(), runtime);
            self.compose_yaml.invalidate();
            self.container_generation = self.container_generation.wrapping_add(1).max(1);
            self.containers.reset(self.container_generation);
            self.compose_projects.clear();
            self.compose_pending_down = None;
            self.compose_pending_context = None;
            self.confirmed_compose_down = None;
            self.container_delete_confirmation = None;
            self.container_inspect_request.cancel_pending();
            self.container_output_request.cancel_pending();
            self.container_inspect.reset();
            self.container_output.reset();
            self.container_output_target_label = None;
            self.arm_container_data_entrance_if_changed(&previous_container_scope, cx);
            cx.emit(OperationsEvent::RootConfirmationChanged);
        }
        self.load_containers(cx);
    }

    /// 根据只读页选中的区块启动恰好一种读取，不请求宿主全量列表。
    ///
    /// @author kongweiguang
    pub(super) fn submit_container_read(
        &mut self,
        target: ContainerTarget,
        kind: ContainerReadKind,
        cx: &mut Context<Self>,
    ) {
        if !self.active || !self.container_read_allowed(&target) {
            return;
        }
        if matches!(
            self.container_panel_context,
            Some(ContainerPanelContext::ContainerInfo { .. })
        ) {
            self.container_info_tab = kind;
        }
        match kind {
            ContainerReadKind::Inspect => self.inspect_container(target, cx),
            ContainerReadKind::Stats => self.inspect_container_output(target, false, cx),
            ContainerReadKind::Logs => self.inspect_container_output(target, true, cx),
        }
    }

    /// 将容器面板上下文投影到已有宿主与 ToolRail 目标字段，不从左栏选择补猜目标。
    ///
    /// @author kongweiguang
    fn apply_container_panel_context(&mut self, context: ContainerPanelContext) {
        if let ContainerPanelContext::ContainerInfo { target, .. } = &context {
            self.runtime = target.runtime;
        }
        let host_id = match &context {
            ContainerPanelContext::Unavailable { .. } => None,
            ContainerPanelContext::Host { host_id, .. } => host_id.clone(),
            ContainerPanelContext::WorkspaceHost { host_id, .. } => Some(host_id.clone()),
            ContainerPanelContext::ContainerInfo { target, .. } => target.host_id.clone(),
        };
        self.tool_rail_target = Some(match &context {
            ContainerPanelContext::Unavailable { .. } => ToolRailOperationTarget::Unavailable,
            ContainerPanelContext::Host {
                host_id: Some(host_id),
                ..
            }
            | ContainerPanelContext::WorkspaceHost { host_id, .. }
            | ContainerPanelContext::ContainerInfo {
                target:
                    ContainerTarget {
                        host_id: Some(host_id),
                        ..
                    },
                ..
            } => ToolRailOperationTarget::Ssh {
                host_id: host_id.clone(),
            },
            ContainerPanelContext::Host { host_id: None, .. }
            | ContainerPanelContext::ContainerInfo {
                target: ContainerTarget { host_id: None, .. },
                ..
            } => ToolRailOperationTarget::Local,
        });
        self.host_target = host_id;
    }

    /// 当前固定容器工具的刷新只读取 Host 清单或详情页选中的单个容器。
    ///
    /// @author kongweiguang
    fn reload_container_context(&mut self, cx: &mut Context<Self>) {
        match self.container_panel_context.as_ref() {
            Some(ContainerPanelContext::Host { .. }) => self.load_containers(cx),
            Some(ContainerPanelContext::WorkspaceHost { .. }) => self.load_containers(cx),
            Some(ContainerPanelContext::ContainerInfo { target, .. }) => {
                self.submit_container_read(target.clone(), self.container_info_tab, cx);
            }
            Some(ContainerPanelContext::Unavailable { .. }) => {}
            None => self.load_containers(cx),
        }
    }

    /// 纯设置首次目标，避免 ToolRail 创建面板时先查询本机再切换到所选 SSH 主机。
    pub fn with_initial_host_target(mut self, host_id: Option<HostId>) -> Self {
        self.host_target = host_id.clone();
        self.server_info_target = host_id.map_or(ServerInfoTarget::Local, |host_id| {
            ServerInfoTarget::Ssh { host_id }
        });
        self
    }

    /// 固定 ToolRail 初次挂载时保留目标来源，避免无目标被初始化成 Local。
    ///
    /// @author kongweiguang
    pub fn with_initial_tool_rail_target(mut self, target: ToolRailOperationTarget) -> Self {
        self.host_target = target.host_id();
        self.server_info_target = self
            .host_target
            .clone()
            .map_or(ServerInfoTarget::Local, |host_id| ServerInfoTarget::Ssh {
                host_id,
            });
        self.tool_rail_target = Some(target);
        self
    }

    /// Ports 首帧携带容器提示，同时把提交目标降为不可用，避免首帧借用宿主或侧栏选择。
    ///
    /// @author kongweiguang
    pub(crate) fn with_initial_ports_container_target(
        mut self,
        target: ToolRailOperationTarget,
        container_target: ContainerTarget,
    ) -> Self {
        let target = resolve_ports_panel_target(target, Some(&container_target));
        self.host_target = target.host_id();
        self.server_info_target = self
            .host_target
            .clone()
            .map_or(ServerInfoTarget::Local, |host_id| ServerInfoTarget::Ssh {
                host_id,
            });
        self.tool_rail_target = Some(target);
        self.tool_rail_container_target = Some(container_target);
        self
    }

    /// ToolRail 按固定槽位提供宽度模式；这里只调整绘制状态，不触发目标读取或任务重排。
    ///
    /// @author kongweiguang
    pub fn set_compact_layout(&mut self, compact: bool, cx: &mut Context<Self>) {
        if self.compact_layout == compact {
            return;
        }
        self.compact_layout = compact;
        cx.notify();
    }

    /// 注入由 Root 持有并订阅的代理面板，保持工作区会话绑定不由运维页猜测。
    pub fn with_proxy_panel(mut self, panel: Entity<proxy_panel::ProxyApplicationPanel>) -> Self {
        self.proxy_panel = Some(panel);
        self
    }

    /// 供完整工作台工具快捷键切换分区；离开容器区时撤销针对旧行的删除确认。
    ///
    /// @author kongweiguang
    pub fn select_section(&mut self, section: OperationsSection, cx: &mut Context<Self>) {
        if section == OperationsSection::Containers && self.container_panel_context.is_none() {
            return;
        }
        if self.section_selection.select(section) {
            self.scroll_operations_to_top();
            if section != OperationsSection::Containers {
                self.container_delete_confirmation = None;
            }
            self.server.cancel_pending();
            if section == OperationsSection::ServerInfo {
                self.request_immediate_server_sample();
            }
            self.publish_server_monitor_configuration();
            if self.active && self.initialized {
                self.start_visible_services(cx);
                self.reload_non_server(cx);
            } else if !self.section_selection.watches_forwards() {
                self.stop_forward_watch();
            }
            cx.notify();
        }
    }

    /// 隐藏只暂停新的采样并撤销未提交确认；原目标在途读取正常收尾，避免反复开关制造取消错误。
    /// 固定 tmux 已有结果或在途读取时，恢复焦点不额外刷新；首载与原目标的未核对变更仍须读取。
    ///
    /// @author kongweiguang
    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        if !active {
            self.container_delete_confirmation = None;
            self.compose_pending_down = None;
            self.compose_pending_context = None;
            self.confirmed_compose_down = None;
            self.forward_pending_delete = None;
            // 隐藏时立即停止固定数据区的帧请求；尚未消费的目标等待保留给隐藏期结果。
            self.settle_fixed_data_entrance();
            cx.emit(OperationsEvent::RootConfirmationChanged);
        }
        if active {
            self.request_immediate_server_sample();
            // 隐藏期间到达的写回执在这里显示一次；同一目标重显不重放写操作。
            self.restore_deferred_tmux_feedback();
        }
        self.publish_server_monitor_configuration();
        if !active {
            self.stop_forward_watch();
        } else if self.initialized {
            self.start_visible_services(cx);
            if !self.is_fixed_tmux_view()
                || self.tmux.phase() == state::SectionPhase::NotLoaded
                || (self.tmux.phase() != state::SectionPhase::Loading
                    && self.dirty_targets.tmux_is_dirty(&self.tmux_target()))
            {
                self.reload_non_server(cx);
            }
        }
        cx.notify();
    }

    /// 周期变化只重排下一次采样，不启动并发请求或重建面板任务。
    pub(super) fn set_server_refresh_interval(
        &mut self,
        interval: ServerInfoRefreshInterval,
        cx: &mut Context<Self>,
    ) {
        if self.server_refresh_interval == interval {
            return;
        }
        self.server_refresh_interval = interval;
        self.publish_server_monitor_configuration();
        if self.active && self.initialized && self.server_section_visible() {
            self.start_server_monitor(cx);
        }
        cx.notify();
    }

    /// 容器 exec 终态可能修改镜像内资源；只标记创建时的 host/runtime，当前目标一致才重读。
    pub fn mark_container_target_dirty(
        &mut self,
        host_id: Option<HostId>,
        runtime: ContainerRuntime,
        cx: &mut Context<Self>,
    ) {
        self.dirty_targets
            .mark_container_dirty(host_id.clone(), runtime);
        if self.host_target == host_id && self.runtime == runtime {
            self.load_containers(cx);
        } else {
            cx.notify();
        }
    }

    /// 根视图显式传入 host；隐藏视图只记录目标并取消旧请求，激活后再读取新目标。
    ///
    /// @author kongweiguang
    pub fn set_host_target(&mut self, host_id: Option<HostId>, cx: &mut Context<Self>) {
        self.set_target_context(host_id, None, None, cx);
    }

    /// 固定右栏消费 Root 的目标判定；Local 和 Unavailable 即使 host 都为 None 也会失效旧请求。
    ///
    /// @author kongweiguang
    pub fn set_tool_rail_target(
        &mut self,
        target: ToolRailOperationTarget,
        cx: &mut Context<Self>,
    ) {
        let host_id = target.host_id();
        self.set_target_context(host_id, Some(target), None, cx);
    }

    /// Ports 以容器身份覆盖宿主猜测；同一代次更新目标与提示，拒绝旧对象的迟到回执。
    ///
    /// @author kongweiguang
    pub(crate) fn set_tool_rail_target_with_container_target(
        &mut self,
        target: ToolRailOperationTarget,
        container_target: Option<ContainerTarget>,
        cx: &mut Context<Self>,
    ) {
        let target = resolve_ports_panel_target(target, container_target.as_ref());
        let host_id = target.host_id();
        self.set_target_context(host_id, Some(target), container_target, cx);
    }

    /// 按区块取得固定右栏的数据入场状态；完整工作台不接入局部动效。
    ///
    /// @author kongweiguang
    fn data_entrance(&self, section: OperationsSection) -> &TargetDataEntrance {
        match section {
            OperationsSection::ServerInfo => &self.server_data_entrance,
            OperationsSection::Containers => &self.container_data_entrance,
            OperationsSection::Tmux => &self.tmux_data_entrance,
            OperationsSection::PortForwards => &self.forward_data_entrance,
        }
    }

    /// 只允许当前固定区块修改自己的入场状态，避免其他面板切换推进代次。
    ///
    /// @author kongweiguang
    fn data_entrance_mut(&mut self, section: OperationsSection) -> &mut TargetDataEntrance {
        match section {
            OperationsSection::ServerInfo => &mut self.server_data_entrance,
            OperationsSection::Containers => &mut self.container_data_entrance,
            OperationsSection::Tmux => &mut self.tmux_data_entrance,
            OperationsSection::PortForwards => &mut self.forward_data_entrance,
        }
    }

    /// 目标身份真实变化时登记一次入场等待；同一目标换 pane 或普通刷新不调用。
    ///
    /// 只有固定区块参与；隐藏面板仍登记代次，但成功结果会以静止方式消费，
    /// 重新显示时不会补播跨隐藏期的动画。
    ///
    /// @author kongweiguang
    fn arm_fixed_data_entrance(
        &mut self,
        section: OperationsSection,
        has_target: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.section_selection.is_fixed() || self.section_selection.active() != section {
            return;
        }
        let allow_motion = motion_allowed(cx.reduce_motion());
        self.data_entrance_mut(section)
            .target_changed(has_target, allow_motion, Instant::now());
    }

    /// 当前目标首个有效成功结果消费入场；失败、取消和旧代次不调用。
    ///
    /// 隐藏期间成功也要消费，避免重新显示后把已存在的数据再次压暗。
    ///
    /// @author kongweiguang
    pub(super) fn complete_fixed_data_entrance(
        &mut self,
        section: OperationsSection,
        cx: &mut Context<Self>,
    ) {
        if !self.section_selection.is_fixed() || self.section_selection.active() != section {
            return;
        }
        let entrance = self.data_entrance(section);
        let epoch = entrance.epoch();
        let animate = self.active
            && entrance.awaiting_result()
            && entrance.allows_motion()
            && motion_allowed(cx.reduce_motion());
        self.data_entrance_mut(section)
            .ready(epoch, Instant::now(), animate);
    }

    /// 固定容器只在 Root 提供宿主上下文时拥有可替换的列表数据。
    ///
    /// @author kongweiguang
    fn container_data_entrance_has_target(&self) -> bool {
        match self.container_panel_context.as_ref() {
            Some(ContainerPanelContext::Host { host_id, .. }) => host_id == &self.host_target,
            Some(ContainerPanelContext::WorkspaceHost { host_id, .. }) => {
                Some(host_id) == self.host_target.as_ref()
            }
            Some(
                ContainerPanelContext::ContainerInfo { .. }
                | ContainerPanelContext::Unavailable { .. },
            )
            | None => false,
        }
    }

    /// 容器目标身份变化后登记入场；同一宿主上下文切换或普通刷新不重播。
    ///
    /// @author kongweiguang
    fn arm_container_data_entrance_if_changed(
        &mut self,
        previous_scope: &(Option<HostId>, ContainerRuntime),
        cx: &mut Context<Self>,
    ) {
        if previous_scope == &(self.host_target.clone(), self.runtime) {
            return;
        }
        self.arm_fixed_data_entrance(
            OperationsSection::Containers,
            self.container_data_entrance_has_target(),
            cx,
        );
    }

    /// 固定数据区按当前入场采样返回局部透明度；非固定工作台始终完整显示。
    ///
    /// @author kongweiguang
    pub(super) fn fixed_data_entrance_opacity(&self) -> f32 {
        if !self.section_selection.is_fixed() {
            return 1.0;
        }
        self.data_entrance(self.section_selection.active())
            .opacity(Instant::now())
    }

    /// 只有固定右栏正在播放入场时才请求后续帧，避免后台轮询空转。
    ///
    /// @author kongweiguang
    pub(super) fn fixed_data_entrance_is_running(&self, now: Instant) -> bool {
        self.section_selection.is_fixed()
            && self
                .data_entrance(self.section_selection.active())
                .is_running(now)
    }

    /// 减少动效时立即落位，但保留尚未消费的目标等待供首个有效结果决定。
    ///
    /// @author kongweiguang
    pub(super) fn settle_fixed_data_entrance(&mut self) {
        if self.section_selection.is_fixed() {
            self.data_entrance_mut(self.section_selection.active())
                .settle();
        }
    }

    /// 目标来源、身份或容器提示改变时撤销瞬时意图并复用代次流程，拒绝旧目标迟到回执。
    ///
    /// @author kongweiguang
    fn set_target_context(
        &mut self,
        host_id: Option<HostId>,
        tool_rail_target: Option<ToolRailOperationTarget>,
        container_target: Option<ContainerTarget>,
        cx: &mut Context<Self>,
    ) {
        if self.host_target == host_id
            && self.tool_rail_target == tool_rail_target
            && self.tool_rail_container_target == container_target
        {
            return;
        }
        // 目标身份变化可能同时影响四种固定数据区；先冻结旧身份，真实变化才登记入场。
        let previous_container_scope = (self.host_target.clone(), self.runtime);
        let previous_tmux_target = self.tmux_target();
        let previous_tmux_available = self.tmux_target_available();
        let previous_server_target = self.server_info_target.clone();
        let previous_available = self.operation_target_available();
        let previous_forward_host =
            resolve_forward_add_host(self.tool_rail_target.as_ref(), self.host_target.as_ref());
        self.host_target = host_id;
        self.tool_rail_target = tool_rail_target;
        self.tool_rail_container_target = container_target;
        self.server_info_target =
            self.host_target
                .as_ref()
                .map_or(ServerInfoTarget::Local, |host_id| ServerInfoTarget::Ssh {
                    host_id: host_id.clone(),
                });
        self.server_generation = self.server_generation.wrapping_add(1).max(1);
        self.target_generation = self.target_generation.wrapping_add(1).max(1);
        self.tmux_view_generation = self.tmux_view_generation.wrapping_add(1).max(1);
        self.container_generation = self.container_generation.wrapping_add(1).max(1);
        self.server.reset(self.server_generation);
        self.containers.reset(self.container_generation);
        self.compose_projects.clear();
        self.compose_pending_down = None;
        self.forward_pending_delete = None;
        self.tmux.reset(self.target_generation);
        self.forwards.reset(self.target_generation);
        self.sync_proxy_forward_projection(cx);
        self.clear_target_details();
        let next_tmux_target = self.tmux_target();
        let next_tmux_available = self.tmux_target_available();
        self.dirty_targets.mark_tmux_dirty(next_tmux_target.clone());
        if previous_tmux_target != next_tmux_target
            || previous_tmux_available != next_tmux_available
        {
            self.arm_fixed_data_entrance(OperationsSection::Tmux, next_tmux_available, cx);
        }
        // 完整运维页没有后续 binding setter 清理 status，可在此消费；固定 tmux 的
        // Root 生产顺序还会紧接着调用 binding setter，若它推进 owner 会先清 status。
        // 为避免一次提示被先消费后清空，固定视图统一交给 binding setter 或渲染前补投。
        if !self.is_fixed_tmux_view()
            && (previous_tmux_target != next_tmux_target
                || (!previous_tmux_available && next_tmux_available))
        {
            self.restore_deferred_tmux_feedback();
        }
        let next_available = self.operation_target_available();
        if previous_server_target != self.server_info_target || previous_available != next_available
        {
            self.arm_fixed_data_entrance(OperationsSection::ServerInfo, next_available, cx);
        }
        let next_forward_host =
            resolve_forward_add_host(self.tool_rail_target.as_ref(), self.host_target.as_ref());
        if previous_forward_host != next_forward_host {
            self.arm_fixed_data_entrance(
                OperationsSection::PortForwards,
                next_available && next_forward_host.is_some(),
                cx,
            );
        }
        self.arm_container_data_entrance_if_changed(&previous_container_scope, cx);
        cx.emit(OperationsEvent::RootConfirmationChanged);
        self.publish_server_monitor_configuration();
        if self.active && self.initialized {
            self.start_visible_services(cx);
            self.reload_non_server(cx);
        }
    }

    /// Dashboard 的 None 仍表示本机；固定工具只有显式 pane 或 HostId 才能发起读取。
    ///
    /// @author kongweiguang
    pub(super) fn operation_target_available(&self) -> bool {
        self.tool_rail_target
            .as_ref()
            .is_none_or(|target| !matches!(target, ToolRailOperationTarget::Unavailable))
    }

    /// 新建 SSH 转发必须有明确 HostId；已保存定义的编辑仍绑定原始 host。
    ///
    /// @author kongweiguang
    pub(super) fn can_add_forward(&self) -> bool {
        resolve_forward_add_host(self.tool_rail_target.as_ref(), self.host_target.as_ref())
            .is_some()
    }

    /// 表单提交只接受当前打开表单的固定 HostId，避免空表单或关闭表单复用陈旧目标。
    ///
    /// @author kongweiguang
    pub(super) fn can_save_forward(&self) -> bool {
        forward_form_can_save(
            self.inputs
                .as_ref()
                .is_some_and(|inputs| inputs.forward_form_open),
            self.forward_form_host_id().as_ref(),
        )
    }

    /// 返回编辑基线主机或 Add 打开时捕获的主机，不读取当前选择以保持表单意图稳定。
    ///
    /// @author kongweiguang
    pub(super) fn forward_form_host_id(&self) -> Option<HostId> {
        resolve_forward_form_host(
            self.forward_edit_expected
                .as_ref()
                .map(|definition| &definition.config.host_id),
            self.forward_add_host_expected.as_ref(),
        )
    }

    /// 面板头部展示真实 target；没有可用目标时给引导，不把占位 Local 展示为结果。
    ///
    /// @author kongweiguang
    pub(super) fn tool_rail_target_label(&self) -> String {
        match self.tool_rail_target.as_ref() {
            Some(ToolRailOperationTarget::Unavailable) => crate::locale::text(
                "无目标 · 选择 SSH 主机或恢复当前终端",
                "No target · select an SSH host or restore the current terminal",
            )
            .to_string(),
            Some(ToolRailOperationTarget::Local) => {
                crate::locale::text("本机终端", "Local terminal").to_string()
            }
            Some(ToolRailOperationTarget::Ssh { host_id }) => format!(
                "SSH · {}",
                self.host_display_names
                    .get(host_id)
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| host_id.as_str().to_owned())
            ),
            None => self.host_target.as_ref().map_or_else(
                || crate::locale::text("本机", "Local").to_string(),
                |host_id| format!("SSH · {}", host_id.as_str()),
            ),
        }
    }

    /// 当前快照就绪才允许按会话 ID 连接；Root 消费事件时复核，防止刷新使旧事件失效。
    ///
    /// @author kongweiguang
    pub(crate) fn tmux_session_attach_allowed(
        &self,
        target: &TmuxTarget,
        session_id: &str,
    ) -> bool {
        self.active
            && !self.busy
            && self.tmux_detach_pending.is_none()
            && self.tmux_target_available()
            && target == &self.tmux_target()
            && self.tmux.phase() == state::SectionPhase::Ready
            && self.tmux.data().is_some_and(|snapshot| {
                snapshot.sessions.iter().any(|session| {
                    session.id == session_id && session.target_ref == snapshot.capability.target_ref
                })
            })
    }

    /// attach 必须同时具备精确 pane binding 与相同的主机/profile，查询目标不能代替写入授权。
    ///
    /// @author kongweiguang
    pub(super) fn tmux_attach_ready(&self) -> bool {
        tmux_attach_binding_matches_target(
            self.tmux_attach_binding
                .as_ref()
                .map(|binding| &binding.target),
            &self.tmux_target(),
        )
    }

    /// 从 Root 接收当前目录的名称投影；更新显示名不能触发切换目标或重建任何会话。
    ///
    /// @author kongweiguang
    pub fn set_host_display_names(
        &mut self,
        names: HashMap<HostId, gpui_kit::SharedString>,
        cx: &mut Context<Self>,
    ) {
        if self.host_display_names == names {
            return;
        }
        self.host_display_names = names;
        cx.notify();
    }

    /// 主机端点为只读展示投影，更新文案不会重建请求、切换目标或覆盖未提交草稿。
    /// @author kongweiguang
    pub(crate) fn set_host_subtitles(
        &mut self,
        subtitles: HashMap<HostId, gpui_kit::SharedString>,
        cx: &mut Context<Self>,
    ) {
        if self.host_subtitles != subtitles {
            self.host_subtitles = subtitles;
            cx.notify();
        }
    }

    /// 当前 tmux 标签取自 focused pane 的真实持久绑定，不以面板选中行冒充已连接会话。
    /// @author kongweiguang
    pub(crate) fn set_tmux_current_session_id(
        &mut self,
        session_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.tmux_current_session_id != session_id {
            self.tmux_current_session_id = session_id;
            cx.notify();
        }
    }

    /// 当前 tmux 名称与 ID 一样只来自 Root 核验的 focused pane 持久绑定。
    /// 名称用于兼容同目标内 ID 变化的当前会话显示，不参与写操作寻址。
    ///
    /// @author kongweiguang
    pub(crate) fn set_tmux_current_session_name(
        &mut self,
        session_name: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.tmux_current_session_name != session_name {
            self.tmux_current_session_name = session_name;
            cx.notify();
        }
    }

    /// 只同步当前目标的运行快照，保留 Root 会话绑定和异步叶子的恢复生命周期。
    ///
    /// @author kongweiguang
    pub(super) fn sync_proxy_forward_projection(&self, cx: &mut Context<Self>) {
        let Some(panel) = self.proxy_panel.clone() else {
            return;
        };
        let host_id = self.host_target.clone();
        let forwards = self
            .forwards
            .data()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                Some(kerminal_application::operations::PortForwardSnapshot {
                    forward: item.forward.clone(),
                    actual_bind: item.actual_bind.clone()?,
                    proxy_url: item.proxy_url.clone(),
                    proxy_application: item.proxy_application?,
                    failure_code: item.failure_code,
                })
            })
            .collect();
        panel.update(cx, move |panel, cx| {
            panel.set_host_forwards(host_id, forwards, cx);
        });
    }

    /// 主题变化只触发语义色板重绘，不改变运维目标和正在执行的用例。
    pub fn set_theme_mode(&mut self, mode: ThemeMode, cx: &mut Context<Self>) {
        self.theme_mode = mode;
        cx.notify();
    }

    /// 接收 Root 的 pane 与 owner/generation 快照；相等时不触发无意义重绘。
    pub fn set_tmux_quickref_binding(
        &mut self,
        binding: Option<TmuxQuickrefTerminalBinding>,
        cx: &mut Context<Self>,
    ) {
        if self.tmux_quickref_binding == binding {
            return;
        }
        self.tmux_quickref_binding = binding;
        cx.notify();
    }

    /// pane 变化只失效 owner 级意图；host/profile 目标变化或失效才重置会话与详情。
    ///
    /// 同一 ssh host 的不同 pane 共享 tmux 服务器，已加载列表和详情可以继续展示；
    /// 视图代次仍必须递增，确保旧 pane 的确认、detach 和迟到 mutation 回执不能改写
    /// 新焦点的 UI。写操作本身仍由原 worker 收尾，不因切换 pane 被取消或重放；
    /// 其反馈按冻结目标暂存并在最终步骤补投，不随被抑制的视图回写一起消失。
    ///
    /// @author kongweiguang
    pub fn set_tmux_attach_binding(
        &mut self,
        binding: Option<TmuxAttachTerminalBinding>,
        cx: &mut Context<Self>,
    ) {
        if self.tmux_attach_binding == binding {
            return;
        }
        let current_binding = self.tmux_attach_binding.clone();
        let previous_target = self.tmux_target();
        let previous_available = self.tmux_target_available();
        self.tmux_attach_binding = binding;
        let next_target = self.tmux_target();
        let next_available = self.tmux_target_available();
        let transition = plan_tmux_attach_transition(
            current_binding.as_ref(),
            self.tmux_attach_binding.as_ref(),
            &previous_target,
            &next_target,
            next_available,
            self.tmux.phase(),
            self.active && self.initialized,
        );
        if transition.owner_changed {
            self.tmux_detach_pending = None;
        }
        if self.is_fixed_tmux_view() && transition.owner_changed {
            self.tmux_view_generation = self.tmux_view_generation.wrapping_add(1).max(1);
            if transition.reset_reads {
                // 目标身份变化或失效时旧快照不再属于当前查询；递增列表代次并中断详情，
                // 避免迟到回执把旧会话、窗口或 capture 应用到新目标。
                self.target_generation = self.target_generation.wrapping_add(1).max(1);
                self.tmux.reset(self.target_generation);
                // 新目标在首次读取前即处于未核对状态；离开旧目标时无需保留历史 dirty。
                self.dirty_targets.mark_tmux_dirty(next_target.clone());
                self.tmux_details.cancel_pending_reads();
                let generation = self.tmux_details.generation.wrapping_add(1).max(1);
                self.tmux_details = TmuxDetailState {
                    generation,
                    ..TmuxDetailState::default()
                };
            }
            self.clear_tmux_dialog();
            self.status = None;
            cx.emit(OperationsEvent::RootConfirmationChanged);
            // 可用性恢复（例如同目标 Local 从无 pane 变为 Ready）不重置读取，但旧画面
            // 曾是“目标不可用”状态；仍应把首个新成功结果当作一次内容替换。首次挂载由
            // 入场自身的未初始化基线吸收，同 pane 普通刷新不会进入这里。
            if previous_target != next_target || previous_available != next_available {
                self.arm_fixed_data_entrance(OperationsSection::Tmux, next_available, cx);
            }
            // 回到原目标或同目标换 pane 后补投一次离开/隐藏期间到达的写回执；这里位于
            // status 清理之后，保证 Root 两步生产顺序不会把刚恢复的提示抹掉。隐藏时
            // restore 自行跳过，留给重新显示时消费。
            self.restore_deferred_tmux_feedback();
            if transition.load_sessions {
                self.load_tmux(cx);
            }
        }
        cx.notify();
    }

    /// 固定 tmux 只服务当前聚焦的 Ready 本机/SSH pane；完整运维页仍使用显式目标。
    ///
    /// @author kongweiguang
    pub(super) fn tmux_target_available(&self) -> bool {
        if !self.is_fixed_tmux_view() {
            return true;
        }
        match self.tool_rail_target.as_ref() {
            Some(ToolRailOperationTarget::Ssh { host_id }) => {
                self.tmux_target().validate().is_ok()
                    && self.tmux_target().host_id.as_ref() == Some(host_id)
            }
            Some(ToolRailOperationTarget::Local) => self.tmux_attach_ready(),
            Some(ToolRailOperationTarget::Unavailable) | None => false,
        }
    }

    /// 判断实例是否为只展示 tmux 的固定工具栏面板，避免改变完整运维页的显式目标语义。
    ///
    /// @author kongweiguang
    fn is_fixed_tmux_view(&self) -> bool {
        self.section_selection.is_fixed()
            && self.section_selection.active() == OperationsSection::Tmux
    }

    /// 固定面板只刷新自己的可取消 section；完整工作台并行刷新所有能力且不互相遮蔽。
    pub(super) fn reload(&mut self, cx: &mut Context<Self>) {
        match self.section_selection.refresh_scope() {
            Some(OperationsSection::ServerInfo) => self.load_server(cx),
            Some(OperationsSection::Containers) => self.reload_container_context(cx),
            Some(OperationsSection::Tmux) => self.load_tmux(cx),
            Some(OperationsSection::PortForwards) => self.load_forwards(cx),
            None => {
                self.load_server(cx);
                self.load_containers(cx);
                self.load_tmux(cx);
                self.load_forwards(cx);
            }
        }
    }

    /// 激活或重新显示时只启动当前分区的读取和适用的状态订阅。
    fn start_visible_services(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        if self.server_section_visible() {
            self.start_server_monitor(cx);
        }
        if self.section_selection.watches_forwards() {
            self.start_forward_watch(cx);
        } else {
            self.stop_forward_watch();
        }
    }

    /// 采样设置变化唤醒唯一 driver，避免每次切换周期都另开轮询任务。
    fn publish_server_monitor_configuration(&mut self) {
        self.server_monitor_revision = self.server_monitor_revision.wrapping_add(1).max(1);
        self.server_monitor_wake.cancel();
        self.server_monitor_wake = CancellationToken::new();
    }

    /// 记录必须尽快采样的状态边沿，即使 driver 忙时未观察到中间的隐藏状态。
    fn request_immediate_server_sample(&mut self) {
        self.server_monitor_immediate_revision = self
            .server_monitor_immediate_revision
            .wrapping_add(1)
            .max(1);
    }

    /// dashboard 离开服务器分区时暂停采集；固定面板永远展示自己的服务器分区。
    fn server_section_visible(&self) -> bool {
        self.operation_target_available()
            && self.section_selection.active() == OperationsSection::ServerInfo
    }

    /// 原子快照设置与对应唤醒令牌，避免配置刚变化后 driver 才订阅新令牌而漏掉唤醒。
    fn server_monitor_configuration(&self) -> ServerMonitorConfiguration {
        ServerMonitorConfiguration {
            revision: self.server_monitor_revision,
            server_generation: self.server_generation,
            immediate_revision: self.server_monitor_immediate_revision,
            panel_active: self.active,
            server_section_visible: self.server_section_visible(),
            interval: self.server_refresh_interval,
            wake: self.server_monitor_wake.clone(),
        }
    }

    /// 自动采样只负责服务器区块，其他能力继续使用自己的目标代次和请求状态。
    fn reload_non_server(&mut self, cx: &mut Context<Self>) {
        match self.section_selection.refresh_scope() {
            Some(OperationsSection::ServerInfo) => {}
            Some(OperationsSection::Containers) => self.reload_container_context(cx),
            Some(OperationsSection::Tmux) => self.load_tmux(cx),
            Some(OperationsSection::PortForwards) => self.load_forwards(cx),
            None => {
                self.load_containers(cx);
                self.load_tmux(cx);
                self.load_forwards(cx);
            }
        }
    }

    /// 宿主或运行时变化时同时释放旧详情和 attach 意图，防止对过期目标执行后续动作。
    ///
    /// @author kongweiguang
    fn clear_target_details(&mut self) {
        self.tmux_detach_pending = None;
        self.tmux_details.cancel_pending_reads();
        self.container_inspect_request.cancel_pending();
        self.container_output_request.cancel_pending();
        self.container_inspect.reset();
        self.container_output.reset();
        self.container_delete_confirmation = None;
        self.container_output_target_label = None;
        self.status = None;
        let generation = self.tmux_details.generation.wrapping_add(1).max(1);
        self.tmux_details = TmuxDetailState {
            generation,
            ..TmuxDetailState::default()
        };
        self.clear_tmux_dialog();
    }

    /// 销毁面板时取消读取票据；普通隐藏保留在途请求，目标变化由各自代次拒绝旧结果。
    ///
    /// @author kongweiguang
    fn cancel_panel_requests(&mut self) {
        self.server.cancel_pending();
        self.containers.cancel_pending();
        self.tmux.cancel_pending();
        self.forwards.cancel_pending();
        if self.container_inspect.phase() == DetailReadPhase::Loading {
            self.container_inspect_request.cancel_pending();
            self.container_inspect.cancel(
                crate::locale::text(
                    "容器详情读取已取消，可再次点击“概览”重试。",
                    "Container details cancelled; click “Overview” to retry.",
                )
                .to_string(),
            );
        }
        if self.container_output.phase() == DetailReadPhase::Loading {
            self.container_output_request.cancel_pending();
            self.container_output.cancel(
                crate::locale::text(
                    "容器输出读取已取消，可再次点击日志或资源按钮重试。",
                    "Container output cancelled; click Logs or Resources to retry.",
                )
                .to_string(),
            );
        }
        self.tmux_details.cancel_pending_reads();
    }
}

impl Drop for OperationsView {
    /// 页面销毁时取消 driver、区块读取和长期订阅；共享 executor 继续负责协作清理。
    fn drop(&mut self) {
        self.cancel_panel_requests();
        self.server_monitor_cancellation.cancel();
        self.server_monitor_wake.cancel();
        self.forward_watch_cancellation.cancel();
        if let Some(cancellation) = self.mutation_cancellation.take() {
            cancellation.cancel();
        }
        if let Some(cancellation) = self.tmux_details.cancellation.take() {
            cancellation.cancel();
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/operations_tmux_binding.rs"]
mod tmux_binding_tests;
