//! 原生工作区根视图与应用服务边界。
//!
//! @author kongweiguang

mod agent_terminal;
mod app_keybindings;
mod canvas_geometry;
mod command_blocks;
mod command_line_projection;
mod command_suggestions;
mod connections;
mod container_panel;
mod context_projection;
mod custom_agent_manager;
mod keybinding_actions;
mod keybinding_focus;
mod keybindings;
mod operations;
pub(crate) mod pane_input;
mod pane_input_runtime;
mod preferences;
mod proxy_runtime;
mod quick_open;
mod quit;
mod services;
mod session_command_pump;
mod session_queue;
mod session_reconnect;
mod snippet_editor;
mod split_target_picker;
#[cfg(feature = "acceptance-metrics")]
mod terminal_acceptance;
pub(crate) mod terminal_context_menu;
mod terminal_ime;
mod terminal_keyboard;
mod terminal_open;
mod terminal_search_overlay;
mod terminal_tool_actions;
mod terminal_tools_runtime;
mod terminal_viewport;
pub(crate) mod terminal_wheel;
mod tool_panels;
mod update_host_recovery;
mod updates;
#[cfg(target_os = "windows")]
mod windows_title_bar;
mod workbench;
mod workbench_drawers;
mod workbench_layout;
mod workbench_resize;
mod workspace_broadcast;
mod workspace_close;
mod workspace_files;
mod workspace_orphan_cleanup;
mod workspace_render;
mod workspace_runtime;
mod workspace_sessions;
mod workspace_tab_creation;
mod workspace_transient;

pub use self::quit::{QuitIntent, RootHostLifecycle, RootViewEvent};
pub use self::services::AppServices;

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use gpui_kit::{
    AnyElement, AnyWindowHandle, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    IntoElement, KeyDownEvent, KeyUpEvent, MouseButton, Render, SharedString, Window,
    WindowAppearance,
    component::{
        button::Button,
        resizable::{ResizablePanelEvent, ResizableState},
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::{
    AppError,
    platform::external_launch::{ExternalActivation, ExternalLaunchOutcome},
    preferences::{
        InterfaceLanguage, SettingsSnapshot, TerminalColorScheme, TerminalSettings, ThemeMode,
        ToolRailToolId,
    },
    sessions::{
        SessionFilter, TerminalCommandAvailability, TerminalCommandContext, UiInputProjection,
        UiSessionBinding, UiTerminalInputRequest,
    },
    workspace::{
        WorkspaceContainerRuntime, WorkspaceLocalMachineScope, WorkspaceMachineStatus,
        WorkspacePaneMode, WorkspaceSessionPane, WorkspaceSessionSnapshot, WorkspaceSessionTab,
        WorkspaceTargetRef,
    },
};
use kerminal_domain::{
    agents::{AgentSessionScope, AgentTargetBinding, AgentTargetLiveStatus},
    commands::SuggestionQueryMode,
    files::{ContainerRuntime, FileEndpoint},
    identity::{HostId, PaneId, SessionId, TabId},
    operations::{ContainerTarget, TmuxTarget},
    preferences::{ConfigurationMode, ConfigurationRecoveryState},
    sessions::{SessionLifecycle, SessionOwner, SessionTarget, TerminalSize},
    workspace::TerminalLayoutNode,
};
use kerminal_terminal::{
    input::{InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey},
    snapshot::{RenderSnapshot, TerminalPalette},
};
use tokio_util::sync::CancellationToken;

use crate::{
    features::connections::HostNavigation,
    features::editor::{FileEditorWorkspace, FileEditorWorkspaceEvent},
    features::preferences::{PreferencesEvent, PreferencesPanel, PreferencesPanelServices},
    features::workspace_panes::{
        SplitCandidatePreview, SplitCandidateProbeContext, WorkspacePaneEvent,
    },
    features::workspace_tabs::{WorkspaceTabEvent, WorkspaceTabs, WorkspaceTabsCreateEvent},
    terminal_canvas::{
        TerminalCanvasFrame, TerminalLinkCandidateId, TerminalLinkDiscoveryCache,
        TerminalLinkSnapshot, TerminalLinkSnapshotIdentity, terminal_canvas,
    },
    terminal_metrics::TerminalMetrics,
    theme::{ThemeTokens, apply_theme_mode, terminal_background_color, terminal_palette_for_mode},
};

use self::pane_input::{PaneTerminalInputEvent, PaneTerminalInputHandler, PaneTextOrigin};
use self::terminal_search_overlay::terminal_search_overlay_element;
use self::updates::{UpdateInstallGuard, UpdateInstallRequest};
use self::workspace_close::{
    WorkspaceDeferredCloseIntent, WorkspaceResourceOperationEntry,
    is_last_committed_workspace_pane, is_removed_uncommitted_workspace_pane,
};
use self::workspace_runtime::{RuntimePanePhase, WorkspaceRuntime};
use crate::features::workspace_controller::{WorkspaceController, WorkspaceControllerEffect};

const SIDEBAR_WIDTH: f32 = 220.0;
const TOOL_RAIL_WIDTH: f32 = 44.0;
const TAB_BAR_HEIGHT: f32 = 38.0;
const STATUS_BAR_HEIGHT: f32 = 26.0;
const RECOVERY_BANNER_HEADER_HEIGHT: f32 = 32.0;
const RECOVERY_ISSUE_ROW_HEIGHT: f32 = 16.0;
const KEY_TEXT_COMMIT_WINDOW: Duration = Duration::from_millis(120);

/// 模态编辑器拥有按键输入时，终端迟到的 plain/IME 事件不能再路由到 pane。
///
/// @author kongweiguang
fn logical_input_modal_blocked(
    preferences_modal_open: bool,
    connections_modal_open: bool,
    snippet_editor_modal_open: bool,
    root_confirmation_open: bool,
    quit_pending: bool,
) -> bool {
    preferences_modal_open
        || connections_modal_open
        || snippet_editor_modal_open
        || root_confirmation_open
        || quit_pending
}

/// Root 合并既有逻辑模态与常驻 Quick Open 的镜像状态，避免为输入门禁重入读取面板 Entity。
/// Quick Open 打开期间任何下层终端输入路径都必须拒绝；显式选择事件会先清镜像再执行动作。
///
/// @author kongweiguang
pub(super) fn terminal_input_modal_blocked(
    existing_modal_blocked: bool,
    quick_open_modal_open: bool,
) -> bool {
    existing_modal_blocked || quick_open_modal_open
}

/// 危险操作的原始业务实体留在各 feature，Root 只协调一个遮罩与输入焦点边界。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootConfirmationOwner {
    Operations,
    FileDelete,
    Editor,
}

struct PendingKeyCommit {
    session_id: SessionId,
    text: String,
    generation: u64,
}

/// 容器 exec 的刷新归属按创建时目标固定，不能在完成时借用面板当前选中的容器。
struct ContainerExecRefreshTarget {
    operations: Entity<crate::features::operations::OperationsView>,
    host_id: Option<HostId>,
    runtime: ContainerRuntime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CenterPanel {
    Terminal,
    Files,
    Editor,
    Preferences,
    Commands,
    Connections,
    McpRuntime,
    Agents,
    Updates,
}

/// 将会话目标投影为 v3 可恢复意图；Telnet/Serial 的运行目标不含 HostId，必须由连接请求补入，
/// 否则不能写出可恢复引用，也不能把已创建的 session 伪装成可持久 pane。
///
/// @author kongweiguang
fn workspace_pane_for_terminal_target(
    target: Option<&SessionTarget>,
    title: &str,
    host_id: Option<&HostId>,
) -> (WorkspaceSessionPane, bool) {
    let mut transient = false;
    let mut pane = WorkspaceSessionPane {
        id: PaneId::new(),
        title: title.to_owned(),
        machine_id: "machine-local".to_owned(),
        mode: WorkspacePaneMode::Local,
        target: Some(WorkspaceTargetRef::Local { profile_id: None }),
        remote_host_id: None,
        container_id: None,
        profile_id: None,
        shell: None,
        args: None,
        cwd: None,
        current_cwd: None,
        env: None,
        remote_command: None,
        tmux_binding: None,
        prompt: String::new(),
        status: WorkspaceMachineStatus::Offline,
        latency_ms: None,
        local_machine_scope: Some(WorkspaceLocalMachineScope::Workspace),
        output_history: None,
    };

    match target {
        None => {}
        Some(SessionTarget::Local(local)) => {
            let shell = local.program.to_str().map(str::to_owned);
            let args = local
                .args
                .iter()
                .map(|argument| argument.to_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>();
            let cwd = local
                .working_directory
                .as_ref()
                .map(|path| path.to_str().map(str::to_owned));
            let env = local
                .environment
                .iter()
                .map(|(key, value)| Some((key.to_str()?.to_owned(), value.to_str()?.to_owned())))
                .collect::<Option<std::collections::BTreeMap<_, _>>>();
            transient = shell.is_none()
                || args.is_none()
                || cwd.as_ref().is_some_and(Option::is_none)
                || env.is_none();
            pane.shell = shell;
            pane.args = args;
            pane.cwd = cwd.flatten();
            pane.env = env;
        }
        Some(SessionTarget::Ssh(ssh)) => {
            pane.machine_id = ssh.host_id.as_str().to_owned();
            pane.mode = WorkspacePaneMode::Ssh;
            pane.target = Some(WorkspaceTargetRef::Ssh {
                host_id: ssh.host_id.clone(),
            });
            pane.remote_host_id = Some(ssh.host_id.clone());
            let cwd = ssh
                .working_directory
                .as_ref()
                .map(|path| path.to_str().map(str::to_owned));
            pane.cwd = cwd.clone().flatten();
            transient = cwd.as_ref().is_some_and(Option::is_none);
            pane.remote_command = match ssh.command.as_ref() {
                None => None,
                Some(kerminal_domain::sessions::SshInitialCommand::Shell(command)) => {
                    Some(command.clone())
                }
                Some(kerminal_domain::sessions::SshInitialCommand::Process { .. }) => {
                    transient = true;
                    None
                }
            };
        }
        Some(SessionTarget::Telnet(_)) if let Some(host_id) = host_id => {
            pane.mode = WorkspacePaneMode::Telnet;
            pane.machine_id = host_id.as_str().to_owned();
            pane.remote_host_id = Some(host_id.clone());
            pane.target = Some(WorkspaceTargetRef::Telnet {
                host_id: host_id.clone(),
            });
        }
        Some(SessionTarget::Telnet(_)) => {
            pane.mode = WorkspacePaneMode::Telnet;
            pane.target = None;
            transient = true;
        }
        Some(SessionTarget::Serial(_)) if let Some(host_id) = host_id => {
            pane.mode = WorkspacePaneMode::Serial;
            pane.machine_id = host_id.as_str().to_owned();
            pane.remote_host_id = Some(host_id.clone());
            pane.target = Some(WorkspaceTargetRef::Serial {
                host_id: host_id.clone(),
            });
        }
        Some(SessionTarget::Serial(_)) => {
            pane.mode = WorkspacePaneMode::Serial;
            pane.target = None;
            transient = true;
        }
        Some(SessionTarget::Container(container)) => {
            pane.mode = WorkspacePaneMode::Container;
            pane.container_id = Some(container.container_id.clone());
            pane.remote_host_id = container.host_id.clone();
            pane.machine_id = container.host_id.as_ref().map_or_else(
                || "machine-local-container".to_owned(),
                |host| host.as_str().to_owned(),
            );
            pane.target =
                container
                    .host_id
                    .as_ref()
                    .map(|host_id| WorkspaceTargetRef::DockerContainer {
                        host_id: host_id.clone(),
                        container_id: container.container_id.clone(),
                        runtime: match container.runtime {
                            kerminal_domain::files::ContainerRuntime::Docker => {
                                WorkspaceContainerRuntime::Docker
                            }
                            kerminal_domain::files::ContainerRuntime::Podman => {
                                WorkspaceContainerRuntime::Podman
                            }
                        },
                        container_name: None,
                        user: None,
                        workdir: None,
                    });
            // ContainerSessionTarget.command 是一次性执行意图，恢复时重放会重复产生副作用。
            transient = true;
        }
    }

    (pane, transient)
}

enum SessionCommand {
    Input {
        session_id: SessionId,
        event: TerminalInputEvent,
    },
    UiInput(Box<session_queue::UiInputWork>),
    Scroll {
        binding: UiSessionBinding,
        lines: i32,
    },
    ScrollTo {
        binding: UiSessionBinding,
        offset: usize,
    },
    SelectionScroll {
        binding: UiSessionBinding,
        offset: usize,
        gesture_id: u64,
        request_id: u64,
    },
    Resize {
        session_id: SessionId,
        size: TerminalSize,
    },
}

/// 输入 Entity 和订阅与一份完整 owner/generation 绑定，防止迟到的 IME 结果改写新 session。
struct PaneInputRegistration {
    binding: UiSessionBinding,
    entity: Entity<PaneTerminalInputHandler>,
    _subscription: gpui_kit::Subscription,
    _viewport_subscription: gpui_kit::Subscription,
    _focus_out_subscription: gpui_kit::Subscription,
}

/// 只在同一 pane 仍活动且原焦点未被用户改变时恢复异步安装的终端焦点。
#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingPaneFocusIntent {
    generation: u64,
    tab_id: TabId,
    pane_id: PaneId,
    session_id: Option<SessionId>,
    restore_generation: Option<u64>,
    expected_focus: Option<FocusHandle>,
}

/// 建议状态只缓存服务返回的可信上下文和其完整 binding，不复制终端输入正文。
struct CachedTerminalCommandContext {
    binding: UiSessionBinding,
    context: TerminalCommandContext,
}

/// Agent 的候选目标只是活动工作区的提示快照；切换 Tab 不会改变已绑定 Agent。
#[derive(Clone, Debug, Eq, PartialEq)]
struct CachedAgentTargetCandidate(Option<crate::features::agents::AgentTargetCandidate>);

/// 终端目标由显式连接意图或 Profile 用例解析，不在视图回调中读取配置文件。
enum TerminalTargetRequest {
    ProfileDefaultLocal,
    ProfileLocal {
        profile_id: String,
    },
    ExplicitHost {
        host_id: HostId,
        target: SessionTarget,
    },
}

/// 初始标签只投影已加载的 Profile 名称；持久化目标仍保留稳定 ID，不从终端内容推断。
///
/// @author kongweiguang
fn terminal_tab_title(
    target_request: &TerminalTargetRequest,
    cached_host_display_name: Option<SharedString>,
) -> SharedString {
    let cached_host_display_name = cached_host_display_name.filter(|name| !name.trim().is_empty());
    match target_request {
        TerminalTargetRequest::ProfileLocal { .. } | TerminalTargetRequest::ProfileDefaultLocal => {
            cached_host_display_name
                .unwrap_or_else(|| crate::locale::text("本地终端", "Local terminal"))
        }
        TerminalTargetRequest::ExplicitHost { target, .. } => match target {
            SessionTarget::Local(_) => crate::locale::text("本地终端", "Local terminal"),
            SessionTarget::Ssh(_) => cached_host_display_name
                .unwrap_or_else(|| crate::locale::text("SSH 终端", "SSH terminal")),
            SessionTarget::Container(_) => cached_host_display_name
                .unwrap_or_else(|| crate::locale::text("容器终端", "Container terminal")),
            SessionTarget::Telnet(_) => crate::locale::text("Telnet 终端", "Telnet terminal"),
            SessionTarget::Serial(_) => crate::locale::text("串口终端", "Serial terminal"),
        },
    }
}

/// 仅把当前协议对应的本地化占位名视为自动标题，避免覆盖用户自定义 pane 名。
///
/// @author kongweiguang
fn is_generated_terminal_pane_title(title: &str, target: Option<&SessionTarget>) -> bool {
    match target {
        Some(SessionTarget::Local(_)) | None => {
            matches!(title, "Terminal" | "本地终端" | "Local terminal")
        }
        Some(SessionTarget::Ssh(_)) => {
            matches!(title, "Terminal" | "SSH 终端" | "SSH terminal")
        }
        Some(SessionTarget::Telnet(_)) => {
            matches!(title, "Terminal" | "Telnet 终端" | "Telnet terminal")
        }
        Some(SessionTarget::Serial(_)) => {
            matches!(title, "Terminal" | "串口终端" | "Serial terminal")
        }
        Some(SessionTarget::Container(_)) => {
            matches!(title, "Terminal" | "容器终端" | "Container terminal")
        }
    }
}

/// 三类运维/恢复终端都复用同一 UI owner 和生命周期安装流程，避免伪装成普通 SSH。
pub(super) enum WorkspaceTerminalOpenKind {
    Direct(SessionTarget),
    ContainerExec {
        target: ContainerTarget,
        command: Vec<String>,
    },
    TmuxAttach {
        target: TmuxTarget,
        session_id: String,
    },
}

/// GPUI 原生终端工作区；每个标签保留自己的 session ID 和不可变渲染快照。
pub struct RootView {
    /// 异步 session ACK 需要回到 Root 所属窗口，以便安装带窗口上下文的 follow-up UI。
    window_handle: AnyWindowHandle,
    services: AppServices,
    /// Close-to-tray is enabled only after the desktop confirms a real tray owner.
    tray_owner_installed: bool,
    is_creating_session: bool,
    status: Option<SharedString>,
    /// 短提示按已观察消息分代，迟到的自动关闭不能清除之后的提示。
    status_toast_revision: u64,
    status_toast_observed: Option<String>,
    configuration_recovery: ConfigurationRecoveryState,
    /// 只缓存已通过 PreferencesService 加载或保存的 typed 快照，供绘制和交互共享。
    settings_snapshot: Option<SettingsSnapshot>,
    window_appearance: WindowAppearance,
    /// 每个 SessionId 独立 FIFO，慢 SSH 写入不会阻塞其它会话的键盘与控制操作。
    session_commands: session_queue::SessionCommandQueues,
    /// 高精度滚轮余量按不可变 pane binding 保存，关闭或换代时清除。
    terminal_wheels: terminal_wheel::TerminalWheelState,
    /// Ready pane 的 snapshot watcher 与 pane 共享取消边界；关闭开始时立即停止后续更新。
    session_watch_cancellations: HashMap<SessionId, CancellationToken>,
    /// 只复用与真实 owner 和 generation 完全一致的输入 handler。
    pane_input_handlers: HashMap<SessionId, PaneInputRegistration>,
    /// 只读绑定在 shell 退出后保留，输入 handler 的撤销不应丢弃历史操作归属。
    terminal_view_bindings: HashMap<SessionId, UiSessionBinding>,
    pane_input_binding_requests: HashMap<SessionId, (u64, SessionOwner)>,
    pane_input_binding_failures: HashSet<SessionId>,
    pane_input_binding_request_generation: u64,
    /// 输入绑定异步完成后，只有原 pane 意图和焦点仍成立才允许恢复焦点。
    pending_pane_focus_intent: Option<PendingPaneFocusIntent>,
    pane_focus_intent_generation: u64,
    /// Rail 显式居中保存只保留一次来源；复用用户导航代次，不为配置重投影建立另一套焦点状态。
    pending_tool_rail_placement_focus: Option<tool_panels::ToolRailPlacementFocusIntent>,
    /// 按键复用上一帧真正采用的紧凑布局与活动工具；只是绘制事实，不另算预算、不保存用户设置。
    tool_rail_rendered_compact: (bool, Option<ToolRailToolId>),
    /// 广播目标保留校验时的完整 UI binding，重连后旧输入必须由 SessionService CAS 拒绝。
    broadcast_session_ids: Option<Vec<UiSessionBinding>>,
    broadcast_generation: u64,
    pending_broadcast_generation: Option<u64>,
    /// 群发草稿、目标和可见性按 Tab 保活，不与 Agent 或后台会话共用选区。
    workspace_broadcast: workspace_broadcast::WorkspaceBroadcastRuntime,
    pending_key_commits: VecDeque<PendingKeyCommit>,
    key_commit_generation: u64,
    /// 只保留当前设置编译的 keymap；配置替换时会清除未完成的 chord 前缀。
    keybinding_runtime: keybindings::RootKeybindingRuntime,
    /// chord 前缀绑定到具体的 GPUI 焦点句柄，防止同一 scope 的控件之间串接。
    keybinding_focus: keybinding_focus::KeybindingFocusTracker<FocusHandle>,
    /// 输入建议候选只由桥接 controller 持有；缓存 context 仅用于绑定和 CAS 版本校验。
    terminal_command_suggestions: command_suggestions::TerminalCommandSuggestionBridge,
    terminal_command_contexts: HashMap<SessionId, CachedTerminalCommandContext>,
    /// 命令正文仅由同 binding 的 FIFO 输入 ACK 推进，不从终端渲染内容反推。
    command_line_projection: command_line_projection::CommandLineProjection,
    suggestion_menu: Option<Entity<command_suggestions::TerminalCommandSuggestionMenu>>,
    suggestion_menu_binding: Option<UiSessionBinding>,
    _suggestion_menu_subscription: Option<gpui_kit::Subscription>,
    focus_handle: FocusHandle,
    /// 窄窗口工作台抽屉的独立焦点边界；打开前焦点保存在 Root 以便关闭后精确恢复。
    compact_drawer_focus_handle: FocusHandle,
    compact_drawer_return_focus: Option<FocusHandle>,
    compact_host_drawer_open: bool,
    compact_tool_drawer_open: bool,
    /// 短窗中焦点终端工具的临时抽屉态，不修改用户手动折叠偏好。
    /// 设置弹层有独立焦点边界；打开时保留原焦点，以便关闭后回到原工作区控件。
    preferences_modal_focus_handle: FocusHandle,
    preferences_modal_open: bool,
    preferences_modal_return_focus: Option<FocusHandle>,
    preferences_modal_generation: u64,
    /// 主机编辑器和设置互斥使用独立焦点边界；关闭只恢复焦点，不切换工作区或重建会话。
    connections_modal_focus_handle: FocusHandle,
    connections_modal_open: bool,
    /// 信任与外部认证提示和主机编辑共用单一 Root 遮罩与焦点边界，不留在左栏子树里。
    connection_prompt_modal_open: bool,
    connections_modal_return_focus: Option<FocusHandle>,
    connections_modal_generation: u64,
    /// 分组重命名开始时的中心工作区；离开连接页或显式导航后立即作废，防止迟到保存回执劫持导航。
    group_rename_return_surface: Option<CenterPanel>,
    /// 操作/文件危险确认只保存当前 feature owner；表单基线与提交仍由该 Entity 持有。
    root_confirmation_owner: Option<RootConfirmationOwner>,
    root_confirmation_operations_view: Option<Entity<crate::features::operations::OperationsView>>,
    root_confirmation_file_pane: Option<Entity<crate::features::files::FilesPane>>,
    root_confirmation_editor: Option<Entity<FileEditorWorkspace>>,
    root_confirmation_return_focus: Option<FocusHandle>,
    root_confirmation_generation: u64,
    /// 片段编辑器弹层只保存焦点生命周期；草稿和保存状态仍由唯一 CommandsView 持有。
    snippet_editor_modal_focus_handle: FocusHandle,
    snippet_editor_modal_open: bool,
    snippet_editor_modal_return_focus: Option<FocusHandle>,
    snippet_editor_modal_generation: u64,
    custom_agent_manager: Option<Entity<crate::features::agents::CustomAgentLauncherManager>>,
    custom_agent_manager_subscription: Option<gpui_kit::Subscription>,
    custom_agent_manager_return_focus: Option<FocusHandle>,
    reported_focus_session: Option<SessionId>,
    _focus_in_subscription: gpui_kit::Subscription,
    _focus_out_subscription: gpui_kit::Subscription,
    terminal_tools_runtime: terminal_tools_runtime::TerminalToolsRuntime,
    /// 命令块的异步快照与正文读取按工具 Entity 和 pane generation 隔离。
    command_block_runtime: command_blocks::CommandBlockRuntime,
    center_panel: CenterPanel,
    theme_mode: ThemeMode,
    terminal_metrics: TerminalMetrics,
    terminal_scrollback: usize,
    files_pane: Option<Entity<crate::features::files::FilesPane>>,
    sftp_panel: Option<Entity<crate::features::files::FilesPane>>,
    preferences_panel: Entity<PreferencesPanel>,
    file_editor_workspace: Option<Entity<FileEditorWorkspace>>,
    _editor_confirmation_observer: Option<gpui_kit::Subscription>,
    commands_view: Option<Entity<crate::features::commands::CommandsView>>,
    _snippet_editor_visibility_subscription: Option<gpui_kit::Subscription>,
    _snippet_terminal_fill_subscription: Option<gpui_kit::Subscription>,
    connections_panel: Option<Entity<crate::features::connections::ConnectionsPanel>>,
    operations_view: Option<Entity<crate::features::operations::OperationsView>>,
    _operations_event_subscription: Option<gpui_kit::Subscription>,
    /// 代理面板只读取真实 UI SSH pane 绑定，不从运维页或当前 Tab 猜目标。
    proxy_panel: Option<Entity<crate::features::operations::proxy_panel::ProxyApplicationPanel>>,
    _proxy_panel_event_subscription: Option<gpui_kit::Subscription>,
    proxy_runtime: proxy_runtime::ProxyRuntime,
    /// 首次 SSH 自动代理结论保留原绑定；显式重试只消费应用签发的一次性能力。
    initial_proxy_notice: Option<proxy_runtime::InitialProxyNotice>,
    /// 每个一次性 exec pane 绑定原始容器目标，终态或确认关闭后才刷新该目标。
    container_exec_refresh_targets: HashMap<PaneId, ContainerExecRefreshTarget>,
    agents_panel: Option<Entity<crate::features::agents::AgentsPanel>>,
    /// 仅在候选语义发生变化时通知 Agent panel，避免终端输出刷新造成候选面板重绘风暴。
    agent_target_candidate: CachedAgentTargetCandidate,
    /// Agent 面板固定在右栏；折叠只隐藏视图，不销毁会话或实体。
    right_dock_collapsed: bool,
    agent_terminal_tools: Option<Entity<crate::features::terminal_tools::TerminalToolsView>>,
    _agent_terminal_tools_event_subscription: Option<gpui_kit::Subscription>,
    /// 搜索表单位于工具实体外的画布宿主；局部显隐变化必须同步刷新 Root 的挂载投影。
    _agent_terminal_tools_observer_subscription: Option<gpui_kit::Subscription>,
    /// Agent 右栏工具区独立折叠，避免联动中央终端工具的可见状态。
    updates_panel: Entity<crate::features::updates::UpdatesPanel>,
    /// Quick Open overlay is always mounted; its view owns modal focus and renders zero-size while closed.
    quick_open_view: Option<Entity<crate::features::quick_open::QuickOpenView>>,
    /// Quick Open 在子 Entity 事件回调期间不可安全 read；Root 用镜像状态阻断迟到的终端输入。
    quick_open_modal_open: bool,
    _quick_open_event_subscription: Option<gpui_kit::Subscription>,
    /// MCP panel only observes and controls the desktop composition root's shared runtime owner.
    mcp_runtime_panel: Option<Entity<crate::features::mcp_runtime::McpRuntimePanel>>,
    selected_agent_session_id: Option<kerminal_domain::identity::AgentSessionId>,
    agent_terminal_session_id: Option<SessionId>,
    agent_terminal_snapshot: Option<RenderSnapshot>,
    agent_terminal_size: Option<TerminalSize>,
    /// 从 SessionService 投影读取真实生命周期；None 表示新绑定仍在确认中。
    agent_terminal_lifecycle: Option<SessionLifecycle>,
    /// Agent PTY 点击后明确保留焦点身份，不借用中央工作区当前 Pane。
    focused_agent_terminal: bool,
    /// 显式新建的单次焦点意图；异步首帧必须仍匹配用户原焦点和目标才能消费。
    pending_agent_focus_intent: Option<agent_terminal::PendingAgentFocusIntent>,
    /// 底部工具折叠仅隐藏 Entity；会话、事件订阅和后台任务继续由 Root 保活。
    /// 记录已入队但尚未确认的 Agent 尺寸，避免布局重绘时重复排入同尺寸 RPC。
    pending_agent_resizes: HashMap<SessionId, TerminalSize>,
    /// 同一失败尺寸不随重绘自动重试；尺寸改变或输入归属换代后解除，关闭时随 handler 清理。
    failed_session_resizes: HashMap<SessionId, TerminalSize>,
    agent_watch_generation: u64,
    agent_watch_cancellation: CancellationToken,
    agent_needs_snapshot: bool,
    selected_host_id: Option<HostId>,
    _files_pane_subscriptions: Option<workspace_files::FilesPaneSubscriptions>,
    _sftp_panel_subscriptions: Option<workspace_files::FilesPaneSubscriptions>,
    _editor_event_subscription: Option<gpui_kit::Subscription>,
    _preferences_event_subscription: gpui_kit::Subscription,
    _connections_event_subscription: Option<gpui_kit::Subscription>,
    _connections_editor_observer: Option<gpui_kit::Subscription>,
    host_navigation: Option<Entity<HostNavigation>>,
    _host_navigation_event_subscription: Option<gpui_kit::Subscription>,
    /// 唯一 Root 校验来源身份并接收资源 ACK，选择器本身不启动会话。
    split_target_picker: Entity<split_target_picker::SplitTargetPicker>,
    /// 每次打开使用独立请求身份，旧弹层的关闭和选择不得影响同来源的新请求。
    split_target_picker_generation: u64,
    _split_target_picker_event_subscription: gpui_kit::Subscription,
    _agent_event_subscription: Option<gpui_kit::Subscription>,
    _updates_event_subscription: gpui_kit::Subscription,
    workspace_session_state: Option<kerminal_application::workspace::WorkspaceSessionLoad>,
    workspace_session_error: Option<SharedString>,
    /// 已验证的 v3 typed reducer 与运行态 pane 映射共同决定恢复、布局和持久化。
    workspace_controller: Option<WorkspaceController>,
    workspace_runtime: WorkspaceRuntime,
    /// 非终端 workspace tab 按稳定 TabId 持有文件面板和恢复状态。
    workspace_files: workspace_files::WorkspaceFileRuntime,
    /// 文件入口保存明确的展示来源，活动传输标签不会抢占共享 SFTP 工作台。
    files_workspace_selection: workspace_files::FilesWorkspaceSelection,
    /// 共享 SFTP 工作台使用窗口内固定身份，不进入工作区文档或独立传输资源生命周期。
    sftp_workbench_tab_id: TabId,
    sftp_workbench_tab_open: bool,
    /// 只在批量持久标签全部收到成功回执后关闭工作台；失败批次保留此意图供原目标重试。
    sftp_workbench_close_after_batch: Option<Vec<TabId>>,
    /// 中央导航或 picker 新建表单推进意图代次；连接指纹模态的焦点变化保留本次 SFTP 意图。
    surface_navigation_generation: u64,
    pending_sftp_navigation: Option<connections::SftpNavigationRequest>,
    /// 共享打开与独立传输共用最新意图身份，旧预检不得覆盖后续请求状态。
    latest_sftp_request_id: Option<String>,
    pending_sftp_transfer_navigation: Option<connections::SftpNavigationRequest>,
    /// 仅工作台选择器的专用创建流程可在保存后回投；窗口重建不恢复此一次性意图。
    pending_sftp_host_creation: Option<connections::PendingSftpHostCreation>,
    /// orphan session 的可见重试入口只投影 WorkspaceRuntime 中保留的实际资源 owner。
    workspace_orphan_cleanup_panel:
        Option<Entity<workspace_orphan_cleanup::WorkspaceOrphanCleanupPanel>>,
    _workspace_orphan_cleanup_subscription: Option<gpui_kit::Subscription>,
    /// 工作区标签和递归分屏 Entity 按稳定 TabId 持有，保存/切换只更新其投影。
    workspace_tabs_view: Entity<WorkspaceTabs>,
    _workspace_tabs_subscription: gpui_kit::Subscription,
    _workspace_tabs_create_subscription: gpui_kit::Subscription,
    /// 名称编辑不发业务事件；观察标签实体变更，让工作台按编辑态即时调整标题栏宽度。
    _workspace_tabs_observer_subscription: gpui_kit::Subscription,
    /// 尺寸 Entity 只负责窗口布局；持久意图仍交给 Preferences 的唯一 settings CAS owner。
    workbench_sidebar_resizable_state: Entity<ResizableState>,
    /// 侧栏宽度参与标题栏定位；只在 Kit 确认一次 resize 后通知 Root 重新读取像素宽度。
    _workbench_sidebar_resize_subscription: gpui_kit::Subscription,
    /// 侧栏折叠前的面板宽度；展开时在新布局完成测量后恢复，避免外层分组重算比例。
    workbench_sidebar_restore_width: Option<gpui_kit::Pixels>,
    /// 侧栏切换前的右 Dock 宽度；Dock 暂时卸载时仍保留目标值。
    workbench_dock_restore_width: Option<gpui_kit::Pixels>,
    workbench_sidebar_restore_pending: bool,
    workbench_dock_restore_pending: bool,
    workbench_layout_restore_generation: u64,
    /// 仅关联最新用户意图与收据，不复制 settings revision 或写入状态机。
    workbench_layout_intent: Option<workbench::PendingWorkbenchLayout>,
    /// 独立保留旧请求失败；普通状态消息与 watcher 不能隐藏未解决的布局写入。
    workbench_layout_save_error: Option<SharedString>,
    /// 底部恢复保留逻辑高度，窄窗有效高度不替换目标；隐藏时等重新挂载。
    workbench_bottom_restore_height: Option<gpui_kit::Pixels>,
    workbench_bottom_restore_pending: bool,
    /// 折叠只隐藏主机列表，展开时继续使用同一个 ResizableState 恢复用户宽度。
    host_sidebar_collapsed: bool,
    /// 右侧 dock 折叠时暂不渲染分组，但继续保留其尺寸状态与工具 Entity。
    workbench_dock_resizable_state: Entity<ResizableState>,
    /// 底部工具拖动高度归 Root 持有，窗口变矮时临时夹紧不替换用户原始尺寸。
    workbench_bottom_resizable_state: Entity<ResizableState>,
    /// 每个宿主的工具状态由同一 Root 持有，停靠变化不会复制/重建 feature Entity。
    tool_rail_state: workbench::ToolRailPanelState,
    context_panel: Option<Entity<crate::features::context::ContextPanel>>,
    _context_panel_event_subscription: Option<gpui_kit::Subscription>,
    logs_view: Option<Entity<crate::features::commands::LogsView>>,
    tool_rail_operations_views:
        BTreeMap<ToolRailToolId, Entity<crate::features::operations::OperationsView>>,
    _tool_rail_operations_subscriptions: BTreeMap<ToolRailToolId, gpui_kit::Subscription>,
    /// 固定 Ports 面板持有独立代理投影，避免与完整运维页共享选择和操作回执。
    tool_rail_proxy_panels: BTreeMap<
        ToolRailToolId,
        Entity<crate::features::operations::proxy_panel::ProxyApplicationPanel>,
    >,
    _tool_rail_proxy_panel_subscriptions: BTreeMap<ToolRailToolId, gpui_kit::Subscription>,
    /// 手动终端重连按 pane incarnation 持有尝试和迟到回执门禁。
    session_reconnect: session_reconnect::SessionReconnectController,
    workspace_renders: HashMap<TabId, Entity<workspace_render::WorkspaceRender>>,
    workspace_render_subscriptions: HashMap<TabId, gpui_kit::Subscription>,
    /// 每个真实 pane 使用自己的工具视图，避免拖选/链接事件借用活动 Tab 的临时状态。
    workspace_tools: HashMap<SessionId, Entity<crate::features::terminal_tools::TerminalToolsView>>,
    workspace_tool_subscriptions: HashMap<SessionId, gpui_kit::Subscription>,
    /// 链接索引只随已接受的可见 pane 快照更新；关闭 pane 时随运行态一起回收。
    terminal_link_discovery_caches: HashMap<PaneId, TerminalLinkDiscoveryCache>,
    /// 重试或关闭同一 PaneId 时递增代次，使旧异步回执不能重新绑定已经替换的资源。
    workspace_restore_generations: HashMap<PaneId, u64>,
    /// 只为当前验收进程中新建的本地 pane 暂存一次性 echo probe 身份；普通恢复不会生成标记。
    #[cfg(feature = "acceptance-metrics")]
    pending_fresh_terminal_probes: HashMap<PaneId, u64>,
    workspace_restore_cancellations: HashMap<PaneId, CancellationToken>,
    /// Split/close reducer 的单个 pending resource operation 以稳定 pane 身份关联。
    workspace_resource_operations: HashMap<PaneId, WorkspaceResourceOperationEntry>,
    /// 终端 Tab 确认弹层前捕获一次焦点；取消或成功 Close 事件会消费同一请求代次。
    pending_close_tab_confirmation_focus: Option<workspace_close::PendingCloseTabConfirmationFocus>,
    /// 批量确认的焦点票据按冻结 intent 保留至最后一个资源 ACK，失败重试不重捕。
    pending_close_batch_confirmation_focus:
        Option<workspace_close::PendingCloseBatchConfirmationFocus>,
    pending_close_batch_focus: Option<workspace_close::PendingCloseBatchFocus>,
    /// 有仓储时先等待初次分类读取，避免用户新建覆盖尚未加载的 workspace。
    workspace_load_pending: bool,
    /// 一个外部 activation 同一时刻只跑一次，避免 IPC 重投并发创建重复临时连接。
    pending_external_activations: Vec<ExternalActivation>,
    pending_quit_intent: Option<QuitIntent>,
    /// 准备失败仍属可用工作台；独立保存原错，直到用户取消或真正派发收尾。
    quit_preparation_failure: Option<SharedString>,
    quit_ready_emitted: bool,
    quit_ready_intent: Option<QuitIntent>,
    host_lifecycle: RootHostLifecycle,
    /// 只在向 desktop 派发收尾事件时置位；Stopping 也用于仍可取消的 workspace 保存准备阶段。
    host_shutdown_requested: bool,
    /// 停止投影首次出现时撤销旧输入焦点，此后保留重试按钮的键盘焦点。
    host_shutdown_view_shown: bool,
    /// 宿主失败原因独立于普通任务状态，迟到回执不能抹掉安全退出所需的原始诊断。
    host_failure_message: Option<SharedString>,
    /// executor 只投影 intake 与受管任务数量；任务清零不能替代宿主资源清理回执。
    application_task_snapshot: kerminal_application::executor::ApplicationTaskSnapshot,
    failed_quit_intent: Option<QuitIntent>,
    update_install_guard: UpdateInstallGuard,
    pending_update_install: Option<UpdateInstallRequest>,
    terminal_color_scheme: TerminalColorScheme,
    cursor_blink_enabled: bool,
    cursor_blink_phase_visible: bool,
    pending_palette_update: Option<(TerminalPalette, usize, u64)>,
    palette_update_running: bool,
    /// 只让最新设置请求发布错误状态，旧重配失败不能覆盖后来成功的用户操作。
    palette_update_generation: u64,
    _appearance_subscription: gpui_kit::Subscription,
    /// 原生窗口失活立即结束拖选和组合，不依赖下一次鼠标事件或重绘才能停止后台手势。
    _terminal_activation_subscription: gpui_kit::Subscription,
}

impl RootView {
    /// 在原始容器 exec 目标上记脏；面板切换目标后也不会刷新另一个容器。
    pub(super) fn mark_container_exec_target_dirty(
        &mut self,
        pane_id: &PaneId,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.container_exec_refresh_targets.remove(pane_id) else {
            return;
        };
        target.operations.update(cx, |view, cx| {
            view.mark_container_target_dirty(target.host_id, target.runtime, cx);
        });
    }

    /// 为一次 pane 恢复/重试分配唯一代次；替换前先协作取消旧任务，迟到成功由调用方清理。
    pub(super) fn begin_workspace_restore(&mut self, pane_id: &PaneId) -> (u64, CancellationToken) {
        if let Some(previous) = self.workspace_restore_cancellations.remove(pane_id) {
            previous.cancel();
        }
        let generation = self
            .workspace_restore_generations
            .entry(pane_id.clone())
            .and_modify(|value| *value = value.wrapping_add(1).max(1))
            .or_insert(1);
        let generation = *generation;
        let cancellation = CancellationToken::new();
        self.workspace_restore_cancellations
            .insert(pane_id.clone(), cancellation.clone());
        (generation, cancellation)
    }

    /// 只接受仍拥有同一恢复令牌的异步回执，防止关闭/重试后的旧 PTY 被重新发布。
    pub(super) fn workspace_restore_is_current(&self, pane_id: &PaneId, generation: u64) -> bool {
        self.workspace_restore_generations.get(pane_id) == Some(&generation)
            && self
                .workspace_restore_cancellations
                .get(pane_id)
                .is_some_and(|token| !token.is_cancelled())
    }

    /// 关闭或放弃 pane 时使所有未完成恢复失效，并释放其 executor 子任务；只清除与被取消代次一致的验收标记。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_workspace_restore(&mut self, pane_id: &PaneId) {
        #[cfg(feature = "acceptance-metrics")]
        if self
            .workspace_restore_generations
            .get(pane_id)
            .is_some_and(|generation| {
                self.pending_fresh_terminal_probes.get(pane_id) == Some(generation)
            })
        {
            self.pending_fresh_terminal_probes.remove(pane_id);
        }
        if let Some(cancellation) = self.workspace_restore_cancellations.remove(pane_id) {
            cancellation.cancel();
        }
        self.workspace_restore_generations
            .entry(pane_id.clone())
            .and_modify(|generation| *generation = generation.wrapping_add(1).max(1))
            .or_insert(1);
    }

    /// 仅为当前可见 CreatePane 暴露未提交布局预览；焦点锁在来源 pane，持久快照保持原样。
    ///
    /// @author kongweiguang
    fn pending_workspace_split_preview(
        &self,
        cx: &Context<RootView>,
    ) -> Option<(SplitCandidatePreview, TerminalLayoutNode, PaneId)> {
        if self.host_shutdown_requested
            || self.center_panel != CenterPanel::Terminal
            || logical_input_modal_blocked(
                self.preferences_modal_open,
                self.connections_modal_open,
                self.snippet_editor_modal_open || self.custom_agent_manager.is_some(),
                self.root_confirmation_owner.is_some(),
                self.pending_quit_intent.is_some() || self.quit_ready_intent.is_some(),
            )
            || self.connection_prompt_modal_open
            || self.split_target_picker.read(cx).is_open()
            || self
                .workspace_tabs_view
                .read(cx)
                .is_create_target_picker_open()
        {
            return None;
        }
        let controller = self.workspace_controller.as_ref()?;
        let snapshot = controller.snapshot();
        let crate::features::workspace_controller::WorkspaceResourceOperation::CreatePane {
            operation_id,
            tab_id,
            source_pane_id,
            pane,
        } = controller.pending_resource_operation()?
        else {
            return None;
        };
        let owner = self.workspace_resource_operations.get(&pane.id)?;
        if owner.operation_id() != operation_id || owner.cancel_requested() {
            return None;
        }
        if snapshot.active_tab_id.as_ref() != Some(tab_id) {
            return None;
        }
        let candidate = controller.pending_create_pane_candidate(operation_id)?;
        if candidate.active_tab_id.as_ref() != Some(tab_id) {
            return None;
        }
        let layout = candidate
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == tab_id)
            .and_then(|tab| tab.terminal_layout())
            .cloned()?;
        if !layout.contains_pane(source_pane_id)
            || !layout.contains_pane(&pane.id)
            || !candidate
                .terminal_panes
                .iter()
                .any(|candidate_pane| candidate_pane.id == pane.id)
        {
            return None;
        }
        Some((
            SplitCandidatePreview {
                operation_id: operation_id.clone(),
                tab_id: tab_id.clone(),
                source_pane_id: source_pane_id.clone(),
                pane_id: pane.id.clone(),
            },
            layout,
            source_pane_id.clone(),
        ))
    }

    /// 只在候选仍由精确、未取消的操作持有且空 pane 正待启动时屏蔽鼠标，失败后立即恢复重试入口。
    ///
    /// @author kongweiguang
    fn workspace_split_candidate_interaction_locked(
        &self,
        preview: &SplitCandidatePreview,
    ) -> bool {
        let owned = self
            .workspace_resource_operations
            .get(&preview.pane_id)
            .is_some_and(|entry| {
                entry.operation_id() == preview.operation_id && !entry.cancel_requested()
            });
        let pending = self
            .workspace_runtime
            .pane(&preview.pane_id)
            .is_some_and(|pane| {
                pane.tab_id == preview.tab_id
                    && pane.phase == RuntimePanePhase::RestorePending
                    && pane.session_id.is_none()
            });
        let has_orphan = self
            .workspace_runtime
            .orphan_cleanup_for_pane(&preview.pane_id)
            .next()
            .is_some();
        owned && pending && !has_orphan
    }

    /// 两个 Root 投影入口共用同一布局/焦点选择，防止临时候选泄漏进持久快照或换走来源焦点。
    ///
    /// @author kongweiguang
    fn workspace_render_layout_projection(
        snapshot: &WorkspaceSessionSnapshot,
        tab_id: &TabId,
        committed_layout: &TerminalLayoutNode,
        pending_split: Option<&(SplitCandidatePreview, TerminalLayoutNode, PaneId)>,
    ) -> Option<(TerminalLayoutNode, PaneId, Option<SplitCandidatePreview>)> {
        if let Some((preview, candidate_layout, source_pane_id)) =
            pending_split.filter(|(preview, _, _)| &preview.tab_id == tab_id)
        {
            if snapshot.active_tab_id.as_ref() != Some(tab_id)
                || !candidate_layout.contains_pane(source_pane_id)
                || !candidate_layout.contains_pane(&preview.pane_id)
            {
                return None;
            }
            return Some((
                candidate_layout.clone(),
                source_pane_id.clone(),
                Some(preview.clone()),
            ));
        }

        let pane_ids = committed_layout.pane_ids();
        let focused = snapshot
            .focused_pane_id
            .as_ref()
            .filter(|pane_id| pane_ids.contains(pane_id))
            .cloned()
            .or_else(|| pane_ids.first().cloned())?;
        Some((committed_layout.clone(), focused, None))
    }

    /// 为 CreatePane 资源操作注册唯一候选 pane，并沿用 workspace restore 的目标解析与 open 管线。
    ///
    /// 候选布局由 controller 持有，在真实 session Ready ACK 前不发布到运行态布局或标签投影。
    ///
    /// @author kongweiguang
    pub(super) fn begin_workspace_split(
        &mut self,
        operation: crate::features::workspace_controller::WorkspaceResourceOperation,
        cx: &mut Context<RootView>,
    ) {
        let crate::features::workspace_controller::WorkspaceResourceOperation::CreatePane {
            operation_id,
            tab_id,
            source_pane_id,
            pane,
        } = operation
        else {
            self.status = Some("工作区资源操作类型无效。".into());
            cx.notify();
            return;
        };
        let pane = *pane;

        let Some(source) = self.workspace_runtime.pane(&source_pane_id) else {
            self.fail_workspace_split_admission(
                &operation_id,
                &pane.id,
                "分屏来源已失效，无法启动新 pane。".into(),
                cx,
            );
            return;
        };
        if source.tab_id != tab_id
            || source.phase != self::workspace_runtime::RuntimePanePhase::Ready
            || source.session_id.is_none()
        {
            self.fail_workspace_split_admission(
                &operation_id,
                &pane.id,
                "分屏来源尚未就绪，无法启动新 pane。".into(),
                cx,
            );
            return;
        }
        match self.workspace_runtime.pane(&pane.id) {
            Some(existing)
                if existing.tab_id == tab_id
                    && existing.persisted == pane
                    && existing.session_id.is_none()
                    && matches!(
                        existing.phase,
                        self::workspace_runtime::RuntimePanePhase::RestorePending
                    ) => {}
            Some(_) => {
                self.fail_workspace_split_admission(
                    &operation_id,
                    &pane.id,
                    "分屏 pane 身份已被其它资源占用。".into(),
                    cx,
                );
                return;
            }
            None => {
                if self
                    .workspace_runtime
                    .register_pending_pane(tab_id.clone(), pane.clone())
                    .is_err()
                {
                    self.fail_workspace_split_admission(
                        &operation_id,
                        &pane.id,
                        "分屏 pane 运行态注册失败。".into(),
                        cx,
                    );
                    return;
                }
            }
        }

        if self
            .workspace_runtime
            .orphan_cleanup_for_pane(&pane.id)
            .next()
            .is_some()
        {
            self.fail_workspace_split_admission(
                &operation_id,
                &pane.id,
                "分屏仍有未核对的资源归属，不能安全重试。".into(),
                cx,
            );
            return;
        }

        match self.workspace_resource_operations.get(&pane.id) {
            Some(entry) if entry.operation_id() == operation_id && entry.cancel_requested() => {
                self.status = Some(crate::locale::text(
                    "分屏关闭仍在等待资源清理，不能重新启动。",
                    "The split is still waiting for resource cleanup and cannot be restarted.",
                ));
                self.refresh_workspace_views(cx);
                cx.notify();
                return;
            }
            Some(entry) if entry.operation_id() == operation_id => {}
            Some(_) => {
                self.fail_workspace_split_admission(
                    &operation_id,
                    &pane.id,
                    "分屏 pane 仍由另一资源操作持有。".into(),
                    cx,
                );
                return;
            }
            None => {
                self.workspace_resource_operations.insert(
                    pane.id.clone(),
                    self::workspace_close::WorkspaceResourceOperationEntry::new(
                        operation_id.clone(),
                    ),
                );
            }
        }
        self.status = Some(crate::locale::text(
            "正在创建分屏终端…",
            "Creating split terminal…",
        ));
        self.refresh_workspace_views(cx);
    }

    /// 只有 session 首帧成功后才提交 CreatePane 候选，并以 controller 的真实布局更新运行态。
    /// Split 候选已保留用户标签元数据，ACK 后直接沿 CAS 保存该快照，不能再从 pane 标题重建标签。
    ///
    /// @author kongweiguang
    pub(super) fn complete_workspace_split_after_open_ack(
        &mut self,
        tab_id: &TabId,
        pane_id: &PaneId,
        generation: u64,
        cx: &mut Context<RootView>,
    ) {
        if !self.workspace_restore_is_current(pane_id, generation) {
            return;
        }
        let Some(entry) = self.workspace_resource_operations.get(pane_id) else {
            self.set_workspace_split_ack_error(
                pane_id,
                "分屏资源已就绪，但没有匹配的工作区操作记录。".into(),
                cx,
            );
            return;
        };
        let operation_id = entry.operation_id().to_owned();
        if entry.cancel_requested() {
            self.set_workspace_split_ack_error(
                pane_id,
                "分屏关闭正在等待资源清理回执；不能提交已取消的候选。".into(),
                cx,
            );
            return;
        }
        let pending_matches = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.pending_resource_operation())
            .is_some_and(|operation| {
                matches!(
                    operation,
                    crate::features::workspace_controller::WorkspaceResourceOperation::CreatePane {
                        operation_id: pending_id,
                        tab_id: pending_tab,
                        pane,
                        ..
                    } if pending_id == &operation_id && pending_tab == tab_id && pane.id == *pane_id
                )
            });
        if !pending_matches {
            self.set_workspace_split_ack_error(
                pane_id,
                "分屏资源回执与当前工作区操作不匹配；终端仍由运行态保留。".into(),
                cx,
            );
            return;
        }

        let completion = self
            .workspace_controller
            .as_mut()
            .map(|controller| controller.complete_resource_operation(&operation_id, Ok(())));
        if !matches!(
            completion,
            Some(Ok(WorkspaceControllerEffect::Changed { .. }))
        ) {
            self.set_workspace_split_ack_error(
                pane_id,
                "分屏资源已就绪，但工作区候选提交失败；终端仍由运行态保留。".into(),
                cx,
            );
            return;
        }

        let layout = self.workspace_controller.as_ref().and_then(|controller| {
            controller
                .snapshot()
                .terminal_tabs
                .iter()
                .find(|tab| tab.id() == tab_id)
                .and_then(WorkspaceSessionTab::terminal_layout)
                .filter(|layout| layout.contains_pane(pane_id))
                .cloned()
        });
        let Some(layout) = layout else {
            self.set_workspace_split_ack_error(
                pane_id,
                "分屏候选已提交，但 controller 未提供包含该 pane 的布局。".into(),
                cx,
            );
            return;
        };
        self.workspace_runtime
            .register_tab_layout(tab_id.clone(), layout);
        self.workspace_resource_operations.remove(pane_id);
        if self.status.as_ref().is_some_and(|status| {
            matches!(
                status.as_ref(),
                "正在创建分屏终端…" | "Creating split terminal…"
            )
        }) {
            self.status = None;
        }
        self.refresh_workspace_views(cx);
        self.refresh_proxy_panel(cx);
        self.persist_workspace_snapshot(cx);
        cx.notify();
    }

    /// 失败时保留 controller pending operation，供显式重试；不发布未创建成功的候选布局。
    fn fail_workspace_split_admission(
        &mut self,
        operation_id: &str,
        pane_id: &PaneId,
        message: SharedString,
        cx: &mut Context<RootView>,
    ) {
        if let Some(controller) = self.workspace_controller.as_mut() {
            let _ = controller.complete_resource_operation(
                operation_id,
                Err(
                    crate::features::workspace_controller::WorkspaceRestoreFailure::ResourceFailure,
                ),
            );
        }
        if self.workspace_runtime.pane(pane_id).is_some() {
            let _ = self
                .workspace_runtime
                .set_status(pane_id, Some(message.clone()));
        }
        self.status = Some(message);
        self.refresh_workspace_views(cx);
        cx.notify();
    }

    /// 将迟到或无法匹配的成功 ACK 留作可见 pane 资源状态，禁止用虚构布局掩盖失配。
    fn set_workspace_split_ack_error(
        &mut self,
        pane_id: &PaneId,
        message: SharedString,
        cx: &mut Context<RootView>,
    ) {
        if self.workspace_runtime.pane(pane_id).is_some() {
            let _ = self
                .workspace_runtime
                .set_status(pane_id, Some(message.clone()));
        }
        self.status = Some(message);
        self.refresh_workspace_views(cx);
        cx.notify();
    }

    /// 每次 Root 渲染时先协调候选布局的群发栏，再补齐 renderer；Entity 按 TabId 保活以保留 resize 状态。
    ///
    /// @author kongweiguang
    fn ensure_workspace_render_entities(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reconcile_split_target_picker(window, cx);
        self.reconcile_workspace_broadcast(window, cx);
        let live_panes = self
            .workspace_runtime
            .panes()
            .map(|(pane_id, _)| pane_id.clone())
            .collect::<HashSet<_>>();
        self.terminal_link_discovery_caches
            .retain(|pane_id, _| live_panes.contains(pane_id));
        let Some(controller) = self.workspace_controller.as_ref() else {
            self.workspace_renders.clear();
            self.workspace_render_subscriptions.clear();
            return;
        };
        let snapshot = controller.snapshot().clone();
        let pending_split = self.pending_workspace_split_preview(cx);
        let mut live_tabs = std::collections::HashSet::new();
        let mut render_specs = Vec::new();
        for tab in &snapshot.terminal_tabs {
            let WorkspaceSessionTab::Terminal {
                id: tab_id, layout, ..
            } = tab
            else {
                continue;
            };
            let Some((view_layout, focused_pane, preview)) =
                Self::workspace_render_layout_projection(
                    &snapshot,
                    tab_id,
                    layout,
                    pending_split.as_ref(),
                )
            else {
                continue;
            };
            if !view_layout.contains_pane(&focused_pane) {
                continue;
            }
            let candidate_interaction_locked = preview
                .as_ref()
                .is_some_and(|preview| self.workspace_split_candidate_interaction_locked(preview));
            live_tabs.insert(tab_id.clone());
            render_specs.push((
                tab_id.clone(),
                view_layout.clone(),
                focused_pane,
                self.workspace_pane_projections(&view_layout, cx),
                preview,
                candidate_interaction_locked,
            ));
        }

        for (tab_id, layout, focused_pane, projections, preview, candidate_interaction_locked) in
            render_specs
        {
            let source_bounds = preview.as_ref().and_then(|preview| {
                self.workspace_renders.get(&tab_id).and_then(|render| {
                    render
                        .read(cx)
                        .measured_pane_bounds(&preview.source_pane_id, cx)
                })
            });
            let candidate = workspace_render::SplitCandidateRenderContext {
                preview,
                source_bounds,
                interaction_locked: candidate_interaction_locked,
            };
            let projection = workspace_render::WorkspaceRenderProjection {
                layout,
                focused_pane,
                panes: projections,
                candidate,
                host_picker: self.split_target_picker_projection(&tab_id, cx),
            };
            if let Some(render) = self.workspace_renders.get(&tab_id) {
                render.update(cx, |render, cx| {
                    render.set_projection(projection, cx);
                });
                continue;
            }

            let root = cx.weak_entity();
            let render_tab_id = tab_id.clone();
            let callback_tab_id = tab_id.clone();
            let render = cx.new(|cx| {
                workspace_render::WorkspaceRender::new(
                    render_tab_id,
                    projection,
                    move |projection, probe_context, window, pane_cx| match root.update(
                        pane_cx,
                        |root, cx| {
                            root.render_workspace_pane(
                                callback_tab_id.clone(),
                                projection.clone(),
                                probe_context,
                                window,
                                cx,
                            )
                        },
                    ) {
                        Ok(content) => content,
                        Err(_) => div().size_full().into_any_element(),
                    },
                    cx,
                )
            });
            let subscription =
                cx.subscribe_in(&render, window, |root, _render, event, window, cx| {
                    root.handle_workspace_render_event(event.clone(), window, cx);
                });
            self.workspace_renders.insert(tab_id.clone(), render);
            self.workspace_render_subscriptions
                .insert(tab_id, subscription);
        }

        self.workspace_renders
            .retain(|tab_id, _| live_tabs.contains(tab_id));
        self.workspace_render_subscriptions
            .retain(|tab_id, _| live_tabs.contains(tab_id));
    }

    /// 仅在当前可见终端 tab 的 Ready pane 接受版本一致的 Root 快照，避免隐藏或迟到数据替换链接索引。
    ///
    /// @author kongweiguang
    fn update_terminal_link_discovery_cache(
        &mut self,
        tab_id: &TabId,
        pane_id: &PaneId,
        binding: &UiSessionBinding,
        snapshot: &RenderSnapshot,
        cx: &mut Context<Self>,
    ) {
        let is_visible_terminal_tab = self.center_panel == CenterPanel::Terminal
            && self
                .workspace_controller
                .as_ref()
                .and_then(|controller| controller.snapshot().active_tab_id.as_ref())
                == Some(tab_id);
        if !is_visible_terminal_tab
            || binding.session_id != snapshot.session_id
            || binding.owner
                != (SessionOwner::Ui {
                    tab_id: tab_id.clone(),
                    pane_id: pane_id.clone(),
                })
        {
            return;
        }

        let Some(runtime_pane) = self.workspace_runtime.pane(pane_id) else {
            return;
        };
        let Some(runtime_snapshot) = runtime_pane.snapshot.as_ref() else {
            return;
        };
        if runtime_pane.tab_id != *tab_id
            || runtime_pane.phase != RuntimePanePhase::Ready
            || runtime_pane.session_id.as_ref() != Some(&binding.session_id)
            || runtime_snapshot.session_id != snapshot.session_id
            || runtime_snapshot.revision != snapshot.revision
            || runtime_snapshot.scrollback_offset != snapshot.scrollback_offset
            || runtime_snapshot.size != snapshot.size
            || self
                .pane_input_handlers
                .get(&binding.session_id)
                .is_none_or(|registration| registration.binding != *binding)
        {
            return;
        }

        let previous = self
            .terminal_link_discovery_caches
            .get(pane_id)
            .and_then(TerminalLinkDiscoveryCache::snapshot)
            .cloned();
        let Some(updated) = self
            .terminal_link_discovery_caches
            .entry(pane_id.clone())
            .or_default()
            .update(binding, snapshot)
        else {
            return;
        };
        if previous
            .as_ref()
            .is_none_or(|previous| !Arc::ptr_eq(previous, &updated))
        {
            self.refresh_context_panel_projection(cx);
        }
    }

    /// 统一构造与 layout PaneId 一一对应的只读投影；群发页头只接受同 Tab 的有效输入绑定，候选仅占布局槽。
    /// Pending 不冒充错误，Close owner 单独投影以屏蔽错误恢复入口。
    ///
    /// @author kongweiguang
    fn workspace_pane_projections(
        &self,
        layout: &TerminalLayoutNode,
        cx: &Context<RootView>,
    ) -> Vec<workspace_render::WorkspacePaneProjection> {
        let view_pane_count = layout.pane_ids().len();
        let focused_pane_id = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.snapshot().focused_pane_id.as_ref());
        let pending_operation = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.pending_resource_operation());
        layout
            .pane_ids()
            .into_iter()
            .map(|pane_id| {
                let runtime = self.workspace_runtime.pane(&pane_id);
                let status = workspace_render::workspace_pane_projection_status(
                    runtime.map(|runtime| runtime.phase),
                    runtime.and_then(|runtime| runtime.status.clone()),
                );
                let split_candidate_cancelable = self
                    .workspace_resource_operations
                    .get(&pane_id)
                    .is_some_and(|entry| {
                        matches!(
                            pending_operation,
                            Some(crate::features::workspace_controller::WorkspaceResourceOperation::CreatePane {
                                operation_id,
                                pane,
                                ..
                            }) if pane.id == pane_id
                                && entry.operation_id() == operation_id
                                && !entry.cancel_requested()
                                && !entry.split_measurement_started()
                        )
                    });
                workspace_render::WorkspacePaneProjection {
                    pane_id: pane_id.clone(),
                    title: self.workspace_pane_title(&pane_id, cx),
                    session_id: runtime.and_then(|runtime| runtime.session_id.clone()),
                    target: runtime.and_then(|runtime| runtime.target.clone()),
                    snapshot: runtime.and_then(|runtime| runtime.snapshot.clone()),
                    status,
                    is_closing: workspace_render::workspace_pane_is_closing(
                        runtime.map(|runtime| runtime.phase),
                    ),
                    split_candidate_cancelable,
                    close_operation_pending: workspace_render::close_operation_targets_pane(
                        pending_operation,
                        &pane_id,
                    ),
                    reconnect: self.session_reconnect_projection(&pane_id),
                    restart: self.session_restart_projection(&pane_id),
                    broadcast: runtime
                        .map(|runtime| {
                            self.workspace_broadcast_header_projection(
                                &runtime.tab_id,
                                &pane_id,
                                view_pane_count,
                                focused_pane_id,
                                self.workspace_broadcast_input_binding(&runtime.tab_id, &pane_id)
                                    .is_some(),
                            )
                        })
                        .unwrap_or_default(),
                }
            })
            .collect()
    }

    /// 以用户自定义 pane 名优先，并把本地化自动 Tab 名视为默认值，让本地 pane 仍显示真实 shell。
    ///
    /// @author kongweiguang
    fn workspace_pane_title(&self, pane_id: &PaneId, cx: &Context<RootView>) -> SharedString {
        let runtime = self.workspace_runtime.pane(pane_id);
        let target = runtime.and_then(|runtime| runtime.target.as_ref());
        if let Some(title) = runtime
            .map(|runtime| runtime.persisted.title.trim())
            .filter(|title| !title.is_empty() && !is_generated_terminal_pane_title(title, target))
        {
            return SharedString::from(title.to_owned());
        }

        match target {
            Some(SessionTarget::Ssh(target)) => self
                .host_navigation
                .as_ref()
                .and_then(|navigation| navigation.read(cx).display_name_for(&target.host_id))
                .unwrap_or_else(|| crate::locale::text("SSH 终端", "SSH terminal")),
            Some(SessionTarget::Local(target)) => {
                let shell = std::path::Path::new(&target.program)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| !name.is_empty());
                shell.map_or_else(
                    || crate::locale::text("本地终端", "Local terminal"),
                    |name| SharedString::from(name.to_owned()),
                )
            }
            Some(SessionTarget::Telnet(_)) => crate::locale::text("Telnet 终端", "Telnet terminal"),
            Some(SessionTarget::Serial(_)) => crate::locale::text("串口终端", "Serial terminal"),
            Some(SessionTarget::Container(target)) => target
                .host_id
                .as_ref()
                .and_then(|host_id| {
                    self.host_navigation
                        .as_ref()
                        .and_then(|navigation| navigation.read(cx).display_name_for(host_id))
                })
                .unwrap_or_else(|| crate::locale::text("容器终端", "Container terminal")),
            None => crate::locale::text("本地终端", "Local terminal"),
        }
    }

    /// 为新建或复用的工具实体应用 session 与当前偏好；规则校验提示由工具叶子去重显示。
    ///
    /// @author kongweiguang
    fn configure_terminal_tools(
        &self,
        tools: &Entity<crate::features::terminal_tools::TerminalToolsView>,
        session_id: Option<SessionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let log_available = self.services.session_logs.is_some();
        let command_block_rail_visible = preferences::interaction_policy(self)
            .is_some_and(|policy| policy.command_block_rail_visible(true));
        let keyword_rules = self
            .settings_snapshot
            .as_ref()
            .map(|snapshot| snapshot.terminal.keyword_highlights.clone());
        let theme_mode = self.theme_mode;
        let appearance = window.appearance();
        tools.update(cx, |tools, cx| {
            tools.set_session(session_id, window, cx);
            tools.set_log_available(log_available, cx);
            tools.set_command_block_rail_visible(command_block_rail_visible, cx);
            if let Some(rules) = keyword_rules {
                // The leaf keeps the last valid matcher and owns its deduplicated safe notice.
                let _ = tools.update_keyword_rules(rules, theme_mode, appearance, cx);
            }
        });
    }

    /// 只有焦点仍属于该 pane 的可信 UI owner 时，建议查询和接受才可以消费按键。
    fn is_active_suggestion_binding(&self, binding: &UiSessionBinding) -> bool {
        if self.center_panel != CenterPanel::Terminal || self.focused_agent_terminal {
            return false;
        }
        let SessionOwner::Ui { tab_id, pane_id } = &binding.owner else {
            return false;
        };
        let Some(controller) = self.workspace_controller.as_ref() else {
            return false;
        };
        let snapshot = controller.snapshot();
        snapshot.active_tab_id.as_ref() == Some(tab_id)
            && snapshot.focused_pane_id.as_ref() == Some(pane_id)
            && self.workspace_runtime.session_for_pane(pane_id) == Some(&binding.session_id)
            && self
                .pane_input_handlers
                .get(&binding.session_id)
                .is_some_and(|registration| registration.binding == *binding)
    }

    /// 返回与活动 handler 完全同代且 prompt 可用的服务上下文，拒绝从渲染行重建命令行。
    fn suggestion_context_for_binding(
        &self,
        binding: &UiSessionBinding,
    ) -> Option<TerminalCommandContext> {
        if !self.is_active_suggestion_binding(binding) {
            return None;
        }
        let cached = self.terminal_command_contexts.get(&binding.session_id)?;
        (cached.binding == *binding
            && cached.context.session_id == binding.session_id
            && cached.context.owner == binding.owner
            && cached.context.session_generation == binding.session_generation
            && cached.context.availability == TerminalCommandAvailability::Available
            && cached.context.prompt_ready
            && !cached.context.alternate_screen)
            .then(|| cached.context.clone())
    }

    /// pane 或 session generation 切换时只把当前可信 context 交给建议 controller。
    fn refresh_active_suggestion_context(&mut self, cx: &mut Context<Self>) {
        let active_context = self.active_session_id().and_then(|session_id| {
            let binding = self
                .pane_input_handlers
                .get(&session_id)
                .map(|registration| registration.binding.clone())?;
            self.suggestion_context_for_binding(&binding)
                .map(|context| (binding, context))
        });
        let Some((binding, context)) = active_context else {
            self.terminal_command_suggestions.observe_context(None);
            self.suggestion_menu_binding = None;
            self.sync_suggestion_menu(cx);
            return;
        };
        if self
            .suggestion_menu_binding
            .as_ref()
            .is_some_and(|current| current != &binding)
        {
            self.suggestion_menu_binding = None;
        }
        if !self
            .terminal_command_suggestions
            .observe_context(Some(&context))
        {
            self.suggestion_menu_binding = None;
        }
        self.sync_suggestion_menu(cx);
    }

    /// 同步只读菜单投影；候选和请求代次始终由 bridge 持有，关闭时释放订阅实体。
    fn sync_suggestion_menu(&mut self, cx: &mut Context<Self>) {
        let Some(projection) = self.terminal_command_suggestions.menu_projection() else {
            self.suggestion_menu = None;
            self.suggestion_menu_binding = None;
            self._suggestion_menu_subscription = None;
            return;
        };
        if self.suggestion_menu_binding.is_none() {
            self.suggestion_menu = None;
            self._suggestion_menu_subscription = None;
            return;
        }
        if let Some(menu) = self.suggestion_menu.as_ref() {
            menu.update(cx, |menu, cx| menu.set_projection(projection, cx));
            return;
        }
        let menu = cx.new(|_| command_suggestions::TerminalCommandSuggestionMenu::new(projection));
        let subscription = cx.subscribe(
            &menu,
            |root, source, event: &command_suggestions::TerminalCommandSuggestionMenuEvent, cx| {
                root.handle_suggestion_menu_event(source.clone(), event, cx);
            },
        );
        self.suggestion_menu = Some(menu);
        self._suggestion_menu_subscription = Some(subscription);
    }

    /// 在 application executor 中运行可取消查询，主线程只接受仍为当前 generation 的结果。
    fn run_terminal_suggestion_job(
        &mut self,
        job: command_suggestions::TerminalSuggestionJob,
        cx: &mut Context<Self>,
    ) {
        let generation = job.request_generation();
        let Some(commands) = self.services.commands.clone() else {
            self.terminal_command_suggestions.fail_request(generation);
            self.sync_suggestion_menu(cx);
            return;
        };
        let executor = self.services.executor.clone();
        cx.spawn(async move |this, cx| {
            let result =
                command_suggestions::TerminalCommandSuggestionBridge::run(job, executor, commands)
                    .await;
            let _ = this.update(cx, |root, cx| {
                if !root
                    .terminal_command_suggestions
                    .is_request_current(generation)
                {
                    return;
                }
                match result {
                    Ok(batch) => {
                        root.terminal_command_suggestions.apply_batch(batch);
                    }
                    Err(_) => {
                        root.terminal_command_suggestions.fail_request(generation);
                    }
                }
                root.sync_suggestion_menu(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 菜单事件带原查询代次和 binding；过期候选不会跨 pane，work 从命令队列只装箱一次。
    /// 打开片段面板也算显式导航，旧 SFTP 预检不能在随后完成时把用户拉回文件工作台。
    ///
    /// @author kongweiguang
    fn handle_suggestion_menu_event(
        &mut self,
        source: Entity<command_suggestions::TerminalCommandSuggestionMenu>,
        event: &command_suggestions::TerminalCommandSuggestionMenuEvent,
        cx: &mut Context<Self>,
    ) {
        if !self
            .suggestion_menu
            .as_ref()
            .is_some_and(|menu| menu.entity_id() == source.entity_id())
        {
            return;
        }
        let Some(binding) = self.suggestion_menu_binding.clone() else {
            return;
        };
        let Some(context) = self.suggestion_context_for_binding(&binding) else {
            self.terminal_command_suggestions.clear();
            self.suggestion_menu_binding = None;
            self.sync_suggestion_menu(cx);
            return;
        };
        match event {
            command_suggestions::TerminalCommandSuggestionMenuEvent::Dismiss {
                request_generation,
            } if self
                .terminal_command_suggestions
                .is_request_current(*request_generation) =>
            {
                self.terminal_command_suggestions.dismiss_menu();
                self.suggestion_menu_binding = None;
                self.sync_suggestion_menu(cx);
            }
            command_suggestions::TerminalCommandSuggestionMenuEvent::Retry {
                request_generation,
            } if self
                .terminal_command_suggestions
                .is_request_current(*request_generation) =>
            {
                if let Some(job) = self.terminal_command_suggestions.request_menu(&context) {
                    self.suggestion_menu_binding = Some(binding);
                    self.sync_suggestion_menu(cx);
                    self.run_terminal_suggestion_job(job, cx);
                }
            }
            command_suggestions::TerminalCommandSuggestionMenuEvent::Accept {
                request_generation,
                candidate_id,
                partial_accept,
            } if self
                .terminal_command_suggestions
                .is_request_current(*request_generation) =>
            {
                if !self.session_commands.is_idle(&binding.session_id) {
                    self.status = Some("终端仍有输入等待写入，请稍后接受建议。".into());
                    cx.notify();
                    return;
                }
                let Some(acceptance) = self
                    .terminal_command_suggestions
                    .accept_candidate_for_generation(
                        *request_generation,
                        candidate_id,
                        *partial_accept,
                        &context,
                    )
                else {
                    return;
                };
                if acceptance.activation
                    == kerminal_domain::commands::CommandSuggestionActivation::OpenSnippetPanel
                {
                    if let Some(commands) = self.commands_view.as_ref() {
                        commands.update(cx, |view, cx| view.select_snippets(cx));
                        self.surface_navigation_generation =
                            self.surface_navigation_generation.wrapping_add(1);
                        self.center_panel = CenterPanel::Commands;
                        self.focused_agent_terminal = false;
                        self.set_reported_focus_session(None, cx);
                        self.refresh_commands_session(cx);
                        self.refresh_active_suggestion_context(cx);
                        cx.notify();
                    }
                    return;
                }
                let Some(request) = self.terminal_command_suggestions.acceptance_input_request(
                    &acceptance,
                    0,
                    &context,
                ) else {
                    return;
                };
                let mode = SuggestionQueryMode::Menu;
                if !self.enqueue_session_command(
                    SessionCommand::UiInput(Box::new(session_queue::UiInputWork::new(
                        request,
                        Some(session_queue::SuggestionAcceptanceWrite {
                            acceptance: acceptance.clone(),
                            event_index: 0,
                            mode,
                        }),
                    ))),
                    cx,
                ) {
                    self.terminal_command_suggestions
                        .acceptance_input_failed(&acceptance, 0);
                    self.suggestion_menu_binding = None;
                    self.sync_suggestion_menu(cx);
                } else {
                    self.suggestion_menu_binding = None;
                    self.sync_suggestion_menu(cx);
                }
            }
            _ => {}
        }
    }

    /// 只在同一 FIFO 空闲时开始建议接受；初始 work 只装箱一次，后续编辑仍由前项 ACK 推进。
    ///
    /// @author kongweiguang
    fn enqueue_suggestion_acceptance(
        &mut self,
        binding: UiSessionBinding,
        context: &TerminalCommandContext,
        acceptance: command_suggestions::TerminalSuggestionAcceptance,
        mode: SuggestionQueryMode,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.session_commands.is_idle(&binding.session_id) {
            self.terminal_command_suggestions.clear();
            self.suggestion_menu_binding = None;
            self.sync_suggestion_menu(cx);
            return false;
        }
        let Some(request) =
            self.terminal_command_suggestions
                .acceptance_input_request(&acceptance, 0, context)
        else {
            return false;
        };
        let accepted = self.enqueue_session_command(
            SessionCommand::UiInput(Box::new(session_queue::UiInputWork::new(
                request,
                Some(session_queue::SuggestionAcceptanceWrite {
                    acceptance: acceptance.clone(),
                    event_index: 0,
                    mode,
                }),
            ))),
            cx,
        );
        if !accepted {
            self.terminal_command_suggestions
                .acceptance_input_failed(&acceptance, 0);
        }
        self.suggestion_menu_binding = None;
        self.sync_suggestion_menu(cx);
        accepted
    }

    /// 为 ready pane 组合工具；split candidate 只绘制无输入探针，普通待恢复 pane 继续走原恢复测量。
    /// 搜索控件作为 Canvas 的绝对定位子层挂载，避免显隐或错误反馈改动 PTY 行列测量。
    /// 原位重启期间保留画面但不重新安装旧代次输入，直到新连接完成身份核对。
    ///
    /// @author kongweiguang
    fn render_workspace_pane(
        &mut self,
        tab_id: TabId,
        projection: workspace_render::WorkspacePaneProjection,
        probe_context: Option<SplitCandidateProbeContext>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(probe_context) = probe_context {
            return self.workspace_split_candidate_probe_element(probe_context, cx);
        }
        let (Some(session_id), Some(snapshot)) = (projection.session_id, projection.snapshot)
        else {
            return self.workspace_restore_size_probe_element(tab_id, projection.pane_id, cx);
        };
        if snapshot.session_id != session_id {
            return div()
                .size_full()
                .text_color(ThemeTokens::current(window, cx).error)
                .child("终端快照与分屏会话不匹配")
                .into_any_element();
        }

        let (tools, new_tools_view) = if let Some(tools) = self.workspace_tools.get(&session_id) {
            (tools.clone(), false)
        } else {
            let tools = cx.new(|cx| {
                crate::features::terminal_tools::TerminalToolsView::new(
                    Some(session_id.clone()),
                    window,
                    cx,
                )
            });
            let subscription = cx.subscribe_in(&tools, window, |root, tools, event, window, cx| {
                if !command_blocks::handle_event_from(root, tools.clone(), event, window, cx) {
                    root.handle_terminal_tools_event_from(tools.clone(), event, window, cx);
                }
            });
            self.workspace_tool_subscriptions
                .insert(session_id.clone(), subscription);
            self.workspace_tools
                .insert(session_id.clone(), tools.clone());
            (tools, true)
        };
        self.configure_terminal_tools(&tools, Some(session_id.clone()), window, cx);
        if new_tools_view {
            command_blocks::refresh_from_event(self, tools.clone(), session_id.clone(), cx);
        }
        let (overlays, search_overlay) = tools.update(cx, |tools, _| {
            (tools.render_overlays(&snapshot), tools.search_overlay())
        });
        let pane_id = projection.pane_id;
        // 重启持有旧 binding 的整个期间保持只读，避免重绘把 Starting 阶段的旧代次重新装回。
        let input_handler = if projection
            .restart
            .as_ref()
            .is_some_and(|restart| restart.busy)
        {
            None
        } else {
            self.ensure_pane_input_handler(
                session_id.clone(),
                SessionOwner::Ui {
                    tab_id: tab_id.clone(),
                    pane_id: pane_id.clone(),
                },
                cx,
            )
        };
        let (focus, ime_preview) = if let Some(input) = input_handler.as_ref() {
            let binding = input.read(cx).binding().clone();
            self.update_terminal_link_discovery_cache(&tab_id, &pane_id, &binding, &snapshot, cx);
            let _ = input.update(cx, |handler, _| {
                handler.update_projection(
                    &binding,
                    &snapshot.session_id,
                    Some(snapshot.cursor),
                    snapshot.scrollback_offset,
                    self.terminal_metrics.clone(),
                )
            });
            let input = input.read(cx);
            let preview = input
                .ime_preview()
                .map(|preview| (preview.text, preview.selection_bytes));
            (input.focus_handle(), preview)
        } else {
            (self.focus_handle.clone(), None)
        };
        let ime_composing = input_handler
            .as_ref()
            .is_some_and(|input| input.read(cx).is_composing());
        let blink_visible = !self.cursor_blink_enabled
            || !snapshot.cursor.blinking
            || self.cursor_blink_phase_visible;
        let right_click_behavior = self
            .settings_snapshot
            .as_ref()
            .map(|settings| settings.terminal.right_click_behavior)
            .unwrap_or_default();
        let terminal_body_opacity = ThemeTokens::current(window, cx)
            .materials
            .terminal_body_opacity;
        #[cfg(target_os = "macos")]
        let mac_option_is_meta = preferences::interaction_policy(self)
            .map(|policy| policy.mac_option_is_meta())
            .unwrap_or_else(|| TerminalSettings::default().mac_option_is_meta);
        #[cfg(not(target_os = "macos"))]
        let mac_option_is_meta = false;
        let canvas = terminal_canvas(
            TerminalCanvasFrame {
                theme: ThemeTokens::current(window, cx),
                view_binding: self.terminal_view_bindings.get(&session_id).cloned(),
                snapshot: Some(snapshot.clone()),
                ime_preview,
                mac_option_is_meta,
                metrics: self.terminal_metrics.clone(),
                overlays,
                cursor_blink_phase_visible: blink_visible,
            },
            cx.entity(),
            input_handler.clone(),
            tools,
            focus.clone(),
            right_click_behavior,
        );
        let mut canvas_region = div()
            .id(format!("workspace-terminal-canvas-{}", pane_id.as_str()))
            .size_full()
            .flex_1()
            .relative()
            .min_w(px(0.))
            .min_h(px(0.))
            .bg(terminal_background_color(snapshot.palette).opacity(terminal_body_opacity));
        if input_handler.is_none() {
            let click_tab_id = tab_id.clone();
            let click_pane_id = pane_id.clone();
            // 尚无 handler 时先在按下阶段申请绑定；已有 handler 由 Canvas 校验 owner 后聚焦。
            // 右键菜单不激活 pane，避免开菜单时改变分屏与选择位置。
            canvas_region = canvas_region.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |root, _, window, cx| {
                    root.focus_workspace_pane(
                        click_tab_id.clone(),
                        click_pane_id.clone(),
                        window,
                        cx,
                    );
                }),
            );
        }
        if let Some(input) = input_handler.as_ref() {
            let key_down_input = input.clone();
            let key_up_input = input.clone();
            canvas_region = canvas_region
                .track_focus(&focus)
                .on_key_down(cx.listener(move |root, event, window, cx| {
                    root.handle_pane_key_down(key_down_input.clone(), event, window, cx);
                }))
                .on_key_up(cx.listener(move |root, event, window, cx| {
                    root.handle_pane_key_up(key_up_input.clone(), event, window, cx);
                }));
        }
        canvas_region = canvas_region.child(canvas);
        if let Some(search) = search_overlay {
            let placement = search.read(cx).overlay_placement();
            canvas_region = canvas_region.child(terminal_search_overlay_element(
                search,
                placement,
                &self.terminal_metrics,
            ));
        }
        let mut container = div()
            .id(format!("workspace-terminal-pane-body-{}", pane_id.as_str()))
            .size_full()
            .min_w(px(0.))
            .min_h(px(0.))
            .flex()
            .flex_col();
        let active_binding = input_handler
            .as_ref()
            .map(|input| input.read(cx).binding().clone());
        if ime_composing && let Some(binding) = active_binding.as_ref() {
            self.suspend_suggestions_for_ime(binding, cx);
        }
        let menu_matches_binding = self.suggestion_menu.is_some()
            && active_binding
                .as_ref()
                .is_some_and(|binding| self.suggestion_menu_binding.as_ref() == Some(binding));
        let inline_ghost = active_binding.as_ref().and_then(|binding| {
            self.terminal_command_suggestions
                .inline_ghost()
                .filter(|ghost| &ghost.binding == binding)
        });
        let show_suggestion_overlays = keybinding_focus::suggestion_overlays_visible(
            ime_composing,
            menu_matches_binding,
            inline_ghost.is_some(),
        );
        if show_suggestion_overlays
            && menu_matches_binding
            && let Some(menu) = self.suggestion_menu.as_ref()
        {
            canvas_region = canvas_region.child(
                div()
                    .id(format!("suggestion-menu-{}", pane_id.as_str()))
                    .absolute()
                    .top(px(12.))
                    .right(px(12.))
                    .child(menu.clone()),
            );
        } else if show_suggestion_overlays && let Some(ghost) = inline_ghost {
            canvas_region = canvas_region.child(
                div()
                    .id(format!("suggestion-ghost-{}", pane_id.as_str()))
                    .absolute()
                    .bottom(px(30.))
                    .left(px(12.))
                    .text_color(ThemeTokens::current(window, cx).muted)
                    .child(ghost.text),
            );
        }
        container = container.child(canvas_region);
        container.into_any_element()
    }

    /// 点击 pane 后先同步 controller 的焦点身份，再把按键和 DECFocusReporting 发给该 SessionId。
    ///
    /// @author kongweiguang
    fn focus_workspace_pane(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = self.workspace_controller.as_mut().map(|controller| {
            controller.apply_pane_event(&tab_id, WorkspacePaneEvent::Activate(pane_id.clone()))
        });
        self.clear_pending_pane_focus();
        match result {
            Some(Ok(WorkspaceControllerEffect::Changed { .. })) => {
                self.focused_agent_terminal = false;
                self.center_panel = CenterPanel::Terminal;
                self.refresh_workspace_views(cx);
                self.persist_workspace_snapshot(cx);
            }
            Some(Ok(WorkspaceControllerEffect::Unchanged)) => {
                self.focused_agent_terminal = false;
                self.center_panel = CenterPanel::Terminal;
            }
            Some(Ok(WorkspaceControllerEffect::ResourceOperation(operation))) => {
                self.handle_workspace_resource_operation(operation, cx);
            }
            Some(Err(error)) => {
                self.status = Some(error.public_message().to_owned().into());
                cx.notify();
                return;
            }
            None => return,
        }
        self.pending_key_commits.clear();
        self.request_workspace_pane_focus(tab_id.clone(), pane_id.clone(), window, cx);
        self.cancel_unfocused_pane_compositions(window, cx);
        self.refresh_commands_session(cx);
        self.refresh_active_suggestion_context(cx);
        self.refresh_proxy_panel(cx);
        if self.workspace_runtime.pane(&pane_id).is_some() {
            self.request_workspace_pane_snapshot(tab_id, pane_id, cx);
        }
        cx.notify();
    }

    /// 激活工作区 tab 后同步可见面板、pane 焦点和顶栏 active 投影，避免后台 controller active 泄漏到共享工作台。
    ///
    /// @author kongweiguang
    fn activate_workspace_tab(
        &mut self,
        tab_id: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab = self.workspace_controller.as_ref().and_then(|controller| {
            controller
                .snapshot()
                .terminal_tabs
                .iter()
                .find(|tab| tab.id() == tab_id)
                .cloned()
        });
        let Some(tab) = tab else {
            return;
        };
        self.surface_navigation_generation = self.surface_navigation_generation.wrapping_add(1);
        self.clear_pending_pane_focus();
        self.clear_keybinding_chord();
        self.focused_agent_terminal = false;
        match tab {
            WorkspaceSessionTab::Terminal { layout, .. } => {
                self.center_panel = CenterPanel::Terminal;
                if let Some(pane_id) = layout.pane_ids().into_iter().next() {
                    self.focus_workspace_pane(tab_id.clone(), pane_id, window, cx);
                } else {
                    self.set_reported_focus_session(None, cx);
                }
            }
            WorkspaceSessionTab::SftpTransfer { .. } => {
                self.center_panel = CenterPanel::Files;
                self.files_workspace_selection =
                    workspace_files::FilesWorkspaceSelection::TransferTab(tab_id.clone());
                self.ensure_workspace_file_tab(tab_id, window, cx);
                self.set_reported_focus_session(None, cx);
            }
            WorkspaceSessionTab::WorkspaceFile { .. } => {
                self.center_panel = CenterPanel::Editor;
                self.ensure_workspace_file_tab(tab_id, window, cx);
                self.activate_workspace_file_tab(tab_id, cx);
                self.set_reported_focus_session(None, cx);
            }
            WorkspaceSessionTab::Unsupported { .. } => {
                self.center_panel = CenterPanel::Terminal;
                self.set_reported_focus_session(None, cx);
            }
        }
        if self.center_panel == CenterPanel::Terminal {
            self.request_visible_workspace_snapshots(cx);
        }
        self.cancel_unfocused_pane_compositions(window, cx);
        self.refresh_commands_session(cx);
        self.refresh_active_suggestion_context(cx);
        self.refresh_proxy_panel(cx);
        self.refresh_workspace_views(cx);
        cx.notify();
    }

    /// 处理递归布局事件；最后一个 pane 的关闭复用标签确认门禁，资源回收仍由 controller ACK 提交。
    /// 关闭按钮的瞬时键盘来源只在此核对并归一，controller 不持有焦点句柄，也不能据此跳过 modal/quit 门禁。
    /// 迟到的 RetryPane 事件若已被 ClosePane/CloseTab 接管，只提示回到关闭/清理入口，不重开会话。
    ///
    /// @author kongweiguang
    fn handle_workspace_render_event(
        &mut self,
        event: workspace_render::WorkspaceRenderEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            workspace_render::WorkspaceRenderEvent::RetryPane { tab_id, pane_id } => {
                let Some(pane) = self.workspace_runtime.pane(&pane_id) else {
                    self.status = Some("此分屏已不存在，不能重试。".into());
                    cx.notify();
                    return;
                };
                let pane_tab_id = pane.tab_id.clone();
                let pane_phase = pane.phase;
                let pane_has_session = pane.session_id.is_some();
                let runtime_persisted = pane.persisted.clone();
                if pane_tab_id != tab_id {
                    self.status = Some("分屏重试目标已变化。".into());
                    cx.notify();
                    return;
                }
                let pending_operation = self
                    .workspace_controller
                    .as_ref()
                    .and_then(|controller| controller.pending_resource_operation());
                if workspace_render::close_operation_targets_pane(pending_operation, &pane_id) {
                    self.status = Some(crate::locale::text(
                        "此 pane 的关闭仍待核对；请通过关闭或清理入口继续。",
                        "This pane has a pending close; continue through its close or cleanup action.",
                    ));
                    cx.notify();
                    return;
                }
                if let Some(entry) = self.workspace_resource_operations.get_mut(&pane_id) {
                    let operation = self
                        .workspace_controller
                        .as_ref()
                        .and_then(|controller| controller.retry_pending_resource_operation());
                    let Some(operation) = operation else {
                        self.status = Some(crate::locale::text(
                            "分屏仍在等待回执，或当前没有可安全重试的失败项。",
                            "The split is awaiting its receipt, or no safely retryable failure is available.",
                        ));
                        cx.notify();
                        return;
                    };
                    let (
                        operation_id,
                        pending_tab_id,
                        pending_pane_id,
                        pending_persisted,
                    ) = match &operation {
                        crate::features::workspace_controller::WorkspaceResourceOperation::CreatePane {
                            operation_id,
                            tab_id,
                            pane: candidate,
                            ..
                        } => (
                            operation_id.as_str(),
                            tab_id,
                            &candidate.id,
                            candidate.as_ref().clone(),
                        ),
                        crate::features::workspace_controller::WorkspaceResourceOperation::ClosePane { .. }
                        | crate::features::workspace_controller::WorkspaceResourceOperation::CloseTab { .. } => {
                            self.status = Some(crate::locale::text(
                                "当前失败项不是分屏创建，不能按 pane 重试。",
                                "The failed operation is not a split create and cannot be retried as a pane.",
                            ));
                            cx.notify();
                            return;
                        }
                    };
                    let exact_pending = pending_tab_id == &tab_id
                        && pending_pane_id == &pane_id
                        && entry.operation_id() == operation_id
                        && pending_persisted == runtime_persisted;
                    let has_runtime_owner =
                        self.workspace_runtime.session_for_pane(&pane_id).is_some()
                            || self
                                .workspace_runtime
                                .orphan_cleanup_for_pane(&pane_id)
                                .next()
                                .is_some();
                    if pane_phase != RuntimePanePhase::RestoreFailed
                        || pane_has_session
                        || !exact_pending
                        || entry.cancel_requested()
                        || has_runtime_owner
                        || !entry.reset_split_measurement_for_retry(operation_id)
                    {
                        self.status = Some(crate::locale::text(
                            "分屏资源归属或关闭状态未能核对，保留原记录且未重试。",
                            "Split resource ownership or close state could not be verified; the record is retained without retry.",
                        ));
                        cx.notify();
                        return;
                    }
                    if self
                        .workspace_runtime
                        .rearm_failed_split_for_measurement(&tab_id, &pending_persisted)
                        .is_err()
                    {
                        self.status = Some(crate::locale::text(
                            "失败分屏的目标或运行阶段已变化，未重新启动。",
                            "The failed pane target or runtime phase changed; it was not restarted.",
                        ));
                        cx.notify();
                        return;
                    }
                    self.handle_workspace_resource_operation(operation, cx);
                } else if pane_phase == RuntimePanePhase::RestoreFailed && !pane_has_session {
                    let pending_split_owns_pane = self
                        .workspace_controller
                        .as_ref()
                        .and_then(|controller| controller.pending_resource_operation())
                        .is_some_and(|operation| {
                            matches!(
                                operation,
                                crate::features::workspace_controller::WorkspaceResourceOperation::CreatePane {
                                    pane,
                                    ..
                                } if pane.id == pane_id
                            )
                        });
                    if pending_split_owns_pane {
                        self.status = Some(crate::locale::text(
                            "分屏操作记录缺失，不能绕过首尺寸核对直接重启。",
                            "The split operation owner is missing; it cannot bypass first-size validation.",
                        ));
                        cx.notify();
                        return;
                    }
                    self.restore_workspace_pane(tab_id, runtime_persisted, cx);
                } else {
                    self.status = Some("此分屏当前没有可重试的恢复操作。".into());
                    cx.notify();
                }
            }
            workspace_render::WorkspaceRenderEvent::RefreshPane {
                tab_id,
                pane_id,
                binding,
            } => self.restart_workspace_pane(tab_id, pane_id, binding, cx),
            workspace_render::WorkspaceRenderEvent::ReconnectPane {
                tab_id,
                pane_id,
                binding,
            } => self.start_session_reconnect(tab_id, pane_id, binding, cx),
            workspace_render::WorkspaceRenderEvent::CancelReconnect {
                tab_id: _,
                pane_id,
                binding,
                attempt_id,
            } => self.cancel_session_reconnect(pane_id, binding, attempt_id, cx),
            workspace_render::WorkspaceRenderEvent::CancelSplitCandidate { preview } => {
                self.cancel_unadmitted_workspace_split(&preview, cx);
            }
            workspace_render::WorkspaceRenderEvent::Pane { tab_id, event } => {
                if let WorkspacePaneEvent::CancelSplitTarget {
                    pane_id,
                    direction,
                    before,
                    request_generation,
                } = &event
                {
                    self.cancel_split_target_picker_request(
                        pane_id,
                        *direction,
                        *before,
                        *request_generation,
                        window,
                        cx,
                    );
                    return;
                }
                if matches!(
                    event,
                    WorkspacePaneEvent::ToggleBroadcastTarget { .. }
                        | WorkspacePaneEvent::ShowBroadcast { .. }
                ) {
                    self.handle_workspace_broadcast_header_event(&tab_id, event, window, cx);
                    return;
                }
                // 关闭按钮的原生键盘来源只在这个 UI 边界消费；controller 与资源 executor 仍只收到原 Close。
                let (event, keyboard_close_focus) = match event {
                    WorkspacePaneEvent::CloseFromKeyboard {
                        pane_id,
                        close_focus,
                    } => {
                        if !close_focus.is_focused(window) {
                            return;
                        }
                        (WorkspacePaneEvent::Close(pane_id), Some(close_focus))
                    }
                    event => (event, None),
                };
                if let WorkspacePaneEvent::ChooseSplitTarget {
                    pane_id,
                    direction,
                    before,
                } = &event
                {
                    self.open_split_target_picker(
                        tab_id.clone(),
                        pane_id.clone(),
                        *direction,
                        *before,
                        window,
                        cx,
                    );
                    return;
                }
                if let WorkspacePaneEvent::Activate(pane_id) = event {
                    self.focus_workspace_pane(tab_id, pane_id, window, cx);
                    return;
                }
                if let WorkspacePaneEvent::Close(pane_id) = &event {
                    let committed_layout =
                        self.workspace_controller.as_ref().and_then(|controller| {
                            controller
                                .snapshot()
                                .terminal_tabs
                                .iter()
                                .find(|tab| tab.id() == &tab_id)
                                .and_then(|tab| tab.terminal_layout())
                        });
                    let closes_last_pane =
                        is_last_committed_workspace_pane(committed_layout, pane_id);
                    if closes_last_pane {
                        if self.defer_workspace_split_close_intent(
                            WorkspaceDeferredCloseIntent::Pane {
                                tab_id: tab_id.clone(),
                                pane_id: pane_id.clone(),
                            },
                            false,
                            keyboard_close_focus.as_ref(),
                            None,
                            window,
                            cx,
                        ) {
                            return;
                        }
                        self.request_workspace_tab_close_with_confirmation(
                            tab_id.clone(),
                            window,
                            cx,
                        );
                        return;
                    }
                    if self.defer_workspace_split_close_intent(
                        WorkspaceDeferredCloseIntent::Pane {
                            tab_id: tab_id.clone(),
                            pane_id: pane_id.clone(),
                        },
                        true,
                        keyboard_close_focus.as_ref(),
                        None,
                        window,
                        cx,
                    ) {
                        return;
                    }
                    let committed_layout =
                        self.workspace_controller.as_ref().and_then(|controller| {
                            controller
                                .snapshot()
                                .terminal_tabs
                                .iter()
                                .find(|tab| tab.id() == &tab_id)
                                .and_then(|tab| tab.terminal_layout())
                        });
                    if is_removed_uncommitted_workspace_pane(
                        committed_layout,
                        self.workspace_runtime.pane(pane_id).is_some(),
                        pane_id,
                    ) {
                        return;
                    }
                }
                let result = self
                    .workspace_controller
                    .as_mut()
                    .map(|controller| controller.apply_pane_event(&tab_id, event));
                match result {
                    Some(Ok(WorkspaceControllerEffect::Unchanged)) => {}
                    Some(Ok(WorkspaceControllerEffect::Changed { .. })) => {
                        self.refresh_workspace_views(cx);
                        self.persist_workspace_snapshot(cx);
                    }
                    Some(Ok(WorkspaceControllerEffect::ResourceOperation(operation))) => {
                        if keyboard_close_focus.is_some() {
                            self.handle_workspace_resource_operation_from_window(
                                operation,
                                keyboard_close_focus,
                                window,
                                cx,
                            );
                        } else {
                            self.dispatch_split_resource_operation(operation, window, cx);
                        }
                    }
                    Some(Err(error)) => {
                        self.status = Some(error.public_message().to_owned().into());
                        cx.notify();
                    }
                    None => {
                        self.status = Some("工作区尚未就绪，无法更改分屏。".into());
                        cx.notify();
                    }
                }
            }
        }
    }

    /// 组装窗口级实体与布局状态；右栏与中央文件浏览状态独立，但共用同一服务、传输队列和执行器。
    /// 字体使用 GPUI 已安装清单，临时抽屉和分隔尺寸随 Root 生命周期保存。
    /// 宿主收尾事实初始为空，避免把可取消准备阶段误当成服务已停止。
    /// SFTP 导航与创建回投从空身份开始，持久工作区不能恢复已经失效的异步用户意图。
    ///
    /// @author kongweiguang
    pub fn new(services: AppServices, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let initial_appearance = window.appearance();
        let initial_theme_mode = ThemeMode::Dark;
        apply_theme_mode(initial_theme_mode, initial_appearance, cx);
        let appearance_subscription = cx.observe_window_appearance(window, |view, window, cx| {
            view.window_appearance = window.appearance();
            apply_theme_mode(view.theme_mode, view.window_appearance, cx);
            view.refresh_terminal_keyword_rules(window, cx);
            view.queue_terminal_palette_update(cx);
            cx.notify();
        });
        let focus_handle = cx.focus_handle();
        let compact_drawer_focus_handle = cx.focus_handle();
        let preferences_modal_focus_handle = cx.focus_handle();
        let connections_modal_focus_handle = cx.focus_handle();
        let snippet_editor_modal_focus_handle = cx.focus_handle();
        let focus_in_subscription = cx.on_focus(&focus_handle, window, |view, _, cx| {
            let session_id = view.focused_terminal_session_id();
            view.set_reported_focus_session(session_id, cx);
        });
        let focus_out_subscription = cx.on_blur(&focus_handle, window, |view, _, cx| {
            view.set_reported_focus_session(None, cx);
        });
        let configuration_recovery = services.preferences.recovery_state();
        let workbench_sidebar_resizable_state = cx.new(|_| ResizableState::default());
        // ResizableState 的 Resized 事件在拖动结束或程序化尺寸变更后发出，避免每帧轮询并让标题栏同步最新分隔线位置。
        let workbench_sidebar_resize_subscription = cx.subscribe(
            &workbench_sidebar_resizable_state,
            |_, _, _: &ResizablePanelEvent, cx| cx.notify(),
        );
        let workbench_dock_resizable_state = cx.new(|_| ResizableState::default());
        let workbench_bottom_resizable_state = cx.new(|_| ResizableState::default());
        crate::locale::apply_interface_language(
            InterfaceLanguage::System,
            services.system_locale.as_deref().unwrap_or("en-US"),
        );
        let files_pane = Self::new_files_panel(&services, cx);
        if let Some(pane) = files_pane.as_ref() {
            // 只有共享中央工作台允许挑选主机；右栏和独立传输标签继续保留各自的目标绑定。
            pane.update(cx, |pane, cx| pane.set_host_picker_enabled(true, cx));
        }
        // 右栏只持有当前终端的浏览状态；服务与队列共享，中心双端工作区不随切 Tab 改目录。
        let sftp_panel = Self::new_files_panel(&services, cx);
        let preferences_panel = {
            let preferences = services.preferences.clone();
            let profiles = services.profiles.clone();
            let hosts = services.hosts.clone();
            let vault_keys = services.vault_keys.clone();
            let configuration_sync = services.configuration_sync.clone();
            let configuration_directory = services.configuration_directory.clone();
            let configuration_changes = services.configuration_changes.clone();
            let external_protocol = services.external_protocol.clone();
            let mcp_runtime_available = services.mcp_runtime.is_some();
            let executor = services.executor.clone();
            cx.new(|cx| {
                let services = PreferencesPanelServices {
                    preferences,
                    profiles,
                    vault_keys,
                    configuration_sync,
                    configuration_directory,
                    configuration_changes,
                    external_protocol,
                    mcp_runtime_available,
                    executor,
                };
                let panel = PreferencesPanel::new(services, window, cx);
                match hosts {
                    Some(hosts) => panel.with_host_management_service(hosts),
                    None => panel,
                }
            })
        };
        let preferences_event_subscription = cx.subscribe_in(
            &preferences_panel,
            window,
            |root, _panel, event, window, cx| match event {
                PreferencesEvent::SettingsLoaded(settings) => {
                    root.pending_tool_rail_placement_focus = None;
                    root.apply_settings_snapshot(settings.clone(), window, cx);
                }
                PreferencesEvent::SettingsSaved(settings) => {
                    root.apply_settings_snapshot(settings.clone(), window, cx);
                    root.complete_tool_rail_placement_focus(window, cx);
                }
                PreferencesEvent::CloseRequested => {
                    root.close_preferences_modal(window, cx);
                }
                PreferencesEvent::OpenMcpRuntimeRequested => {
                    root.close_preferences_modal(window, cx);
                    root.show_mcp_runtime(window, cx);
                }
                PreferencesEvent::OpenUpdatesRequested => {
                    root.close_preferences_modal(window, cx);
                    root.set_center_panel(CenterPanel::Updates, window, cx);
                }
                PreferencesEvent::QuitConfirmed => {
                    root.handle_preferences_quit_result(true, cx);
                }
                PreferencesEvent::QuitCancelled => {
                    root.handle_preferences_quit_result(false, cx);
                }
                PreferencesEvent::QuitDeferred { message } => {
                    root.pending_quit_intent = None;
                    root.status = Some(message.clone().into());
                    cx.notify();
                }
                PreferencesEvent::WorkbenchLayoutPersisted { request_id, layout } => {
                    root.accept_workbench_layout_receipt(*request_id, layout.clone(), cx);
                }
                PreferencesEvent::WorkbenchLayoutSaveFailed { message, .. } => {
                    // 旧 A 失败仍阻断末 B；不能按 request_id 不同而忽略 owner 的失败 fence。
                    root.workbench_layout_save_error = Some(message.clone().into());
                    root.pending_quit_intent = None;
                    root.open_preferences_modal(window, cx);
                    cx.notify();
                }
                PreferencesEvent::AgentLauncherSaved(settings) => {
                    root.apply_custom_agent_manager_outcome(
                        crate::features::agents::CustomAgentManagerWriteOutcome::Confirmed(
                            settings.clone(),
                        ),
                        window,
                        cx,
                    );
                }
                PreferencesEvent::AgentLauncherSaveFailed { code, message } => {
                    use crate::features::agents::CustomAgentManagerWriteOutcome as Outcome;
                    let outcome = match code {
                        kerminal_application::PublicErrorCode::Conflict => {
                            Outcome::Conflict(message.clone())
                        }
                        kerminal_application::PublicErrorCode::OutcomeUnknown => {
                            Outcome::OutcomeUnknown(message.clone())
                        }
                        _ => Outcome::Failed(message.clone()),
                    };
                    root.apply_custom_agent_manager_outcome(outcome, window, cx);
                }
                PreferencesEvent::AgentLauncherReadBack {
                    settings,
                    retry_allowed,
                } => {
                    if root
                        .preferences_panel
                        .read(cx)
                        .agent_launcher_settings_for_manager()
                        .is_ok()
                        && let Some(snapshot) = root.preferences_panel.read(cx).settings_snapshot()
                    {
                        root.apply_settings_snapshot(snapshot, window, cx);
                    }
                    root.apply_custom_agent_manager_outcome(
                        crate::features::agents::CustomAgentManagerWriteOutcome::ReadBack {
                            settings: settings.clone(),
                            retry_allowed: *retry_allowed,
                        },
                        window,
                        cx,
                    );
                }
                PreferencesEvent::AgentLauncherReadBackFailed { message } => {
                    root.apply_custom_agent_manager_outcome(
                        crate::features::agents::CustomAgentManagerWriteOutcome::ReadBackFailed(
                            message.clone(),
                        ),
                        window,
                        cx,
                    );
                }
                PreferencesEvent::ToolRailSaveFailed { message } => {
                    root.pending_tool_rail_placement_focus = None;
                    root.status = Some(message.clone().into());
                    root.open_preferences_modal(window, cx);
                    cx.notify();
                }
            },
        );
        let file_editor_workspace = services.file_editor.clone().map(|service| {
            let executor = services.executor.clone();
            let target_name_lookup = crate::features::files::FileTargetNameLookup::new(
                services.hosts.clone(),
                services.operations.clone(),
            );
            cx.new(|cx| FileEditorWorkspace::new(service, executor, target_name_lookup, window, cx))
        });
        let commands_view = services.commands.clone().map(|commands| {
            let executor = services.executor.clone();
            // 共用既有会话服务以保留精确目标绑定；视图不自行创建 runtime 或访问配置文件。
            let snippet_execution = kerminal_application::commands::SnippetExecutionService::new(
                commands.clone(),
                services.sessions.clone(),
            );
            let configuration_validation = services.configuration_validation.clone();
            let configuration_directory = services.configuration_directory.clone();
            let snippet_transfer_files = services.snippet_transfer_files.clone();
            cx.new(|cx| {
                let view =
                    crate::features::commands::CommandsView::new(commands, executor, window, cx)
                        .with_snippet_execution_service(snippet_execution);
                let view = match configuration_validation {
                    Some(service) => view.with_configuration_validation(service),
                    None => view,
                };
                let view = match configuration_directory {
                    Some(service) => view.with_configuration_directory(service),
                    None => view,
                };
                match snippet_transfer_files {
                    Some(service) => view.with_snippet_transfer_files(service),
                    None => view,
                }
            })
        });
        let snippet_editor_visibility_subscription = commands_view.as_ref().map(|commands| {
            cx.subscribe_in(
                commands,
                window,
                |root,
                 _commands,
                 event: &crate::features::commands::SnippetEditorVisibilityChanged,
                 window,
                 cx| {
                    snippet_editor::sync_visibility(
                        root,
                        event.open,
                        event.focus_command_draft,
                        window,
                        cx,
                    );
                },
            )
        });
        let snippet_terminal_fill_subscription = commands_view.as_ref().map(|commands| {
            cx.subscribe_in(commands, window, |root, commands, event, window, cx| {
                snippet_editor::focus_terminal_after_fill(root, commands, event, window, cx);
            })
        });
        let connections_panel = services
            .hosts
            .clone()
            .zip(services.remote_connections.clone())
            .map(|(hosts, remote)| {
                let rdp = services.rdp_launcher.clone();
                let rdp_status = services.rdp_launch_status.clone();
                let serial_ports = services.serial_ports.clone();
                let connection_probe = services.connection_probe.clone();
                let external_launch = services.external_launch.clone();
                let profiles = services.profiles.clone();
                let executor = services.executor.clone();
                let recovery = configuration_recovery.clone();
                let configuration_changes = services.configuration_changes.clone();
                cx.new(|cx| {
                    let panel = crate::features::connections::ConnectionsPanel::new(
                        hosts, remote, rdp, executor, recovery, window, cx,
                    )
                    .with_profiles(profiles, cx);
                    let panel = match external_launch {
                        Some(service) => panel.with_external_launch_service(service),
                        None => panel,
                    };
                    let panel = match configuration_changes {
                        Some(changes) => panel.with_configuration_changes(changes),
                        None => panel,
                    };
                    let panel = match serial_ports {
                        Some(service) => panel.with_serial_port_service(service),
                        None => panel,
                    };
                    let panel = match connection_probe {
                        Some(service) => panel.with_connection_probe_service(service),
                        None => panel,
                    };
                    match rdp_status {
                        Some(service) => panel.with_rdp_launch_status_service(service),
                        None => panel,
                    }
                })
            });
        let host_navigation = services.hosts.clone().map(|hosts| {
            let executor = services.executor.clone();
            let profiles = services.profiles.clone();
            let recovery = configuration_recovery.clone();
            let configuration_changes = services.configuration_changes.clone();
            cx.new(|cx| {
                let navigation = HostNavigation::new(hosts, executor, recovery, window, cx)
                    .with_profiles(profiles, cx);
                match configuration_changes {
                    Some(changes) => navigation.with_configuration_changes(changes),
                    None => navigation,
                }
            })
        });
        let host_navigation_event_subscription = host_navigation.as_ref().map(|navigation| {
            cx.subscribe_in(
                navigation,
                window,
                |root, _navigation, event, window, cx| {
                    connections::handle_host_navigation_event(root, event, window, cx);
                },
            )
        });
        let split_target_picker = cx.new(|cx| {
            split_target_picker::SplitTargetPicker::new(host_navigation.clone(), window, cx)
        });
        let split_target_picker_event_subscription = cx.subscribe_in(
            &split_target_picker,
            window,
            |root, _picker, event, window, cx| {
                root.handle_split_target_picker_event(event.clone(), window, cx);
            },
        );
        let proxy_panel = services.operations.as_ref().and_then(|operations| {
            operations.terminal_proxy().map(|_| {
                cx.new(|_| {
                    crate::features::operations::proxy_panel::ProxyApplicationPanel::new(
                        crate::features::operations::proxy_panel::ProxyPanelProjection::default(),
                    )
                })
            })
        });
        let proxy_panel_event_subscription = proxy_panel.as_ref().map(|panel| {
            cx.subscribe_in(panel, window, |root, source, event, _, cx| {
                root.handle_proxy_panel_event(source.clone(), event, cx);
            })
        });
        let operations_view = services.operations.clone().map(|operations| {
            let executor = services.executor.clone();
            let proxy_panel = proxy_panel.clone();
            cx.new(|_| {
                let view = crate::features::operations::OperationsView::new(operations, executor);
                match proxy_panel {
                    Some(panel) => view.with_proxy_panel(panel),
                    None => view,
                }
            })
        });
        let operations_event_subscription = operations_view.as_ref().map(|operations| {
            cx.subscribe_in(
                operations,
                window,
                |root, operations_view, event, window, cx| {
                    let panes_before = root
                        .workspace_runtime
                        .panes()
                        .map(|(pane_id, _)| pane_id.clone())
                        .collect::<Vec<_>>();
                    if let crate::features::operations::OperationsEvent::RootConfirmationChanged =
                        event
                    {
                        let active = operations_view.read(cx).root_confirmation_active();
                        root.sync_operations_root_confirmation(
                            operations_view.clone(), active, window, cx,
                        );
                    } else if let crate::features::operations::OperationsEvent::ContainerExecRequested {
                        target,
                        ..
                    } = event
                    {
                        let target = target.clone();
                        operations::handle_operations_event(root, event, operations_view, window, cx);
                        if let Some((pane_id, _)) =
                            root.workspace_runtime.panes().find(|(pane_id, pane)| {
                                !panes_before.contains(pane_id)
                                    && pane.phase == RuntimePanePhase::RestorePending
                            })
                        {
                            root.container_exec_refresh_targets.insert(
                                pane_id.clone(),
                                ContainerExecRefreshTarget {
                                    operations: operations_view.clone(),
                                    host_id: target.host_id,
                                    runtime: target.runtime,
                                },
                            );
                        }
                    } else {
                        operations::handle_operations_event(root, event, operations_view, window, cx);
                    }
                },
            )
        });
        let agents_panel = services.agents.clone().map(|agents| {
            let executor = services.executor.clone();
            cx.new(|cx| {
                crate::features::agents::AgentsPanel::new(
                    agents,
                    executor,
                    AgentSessionScope::Global,
                    window,
                    cx,
                )
            })
        });
        let context_panel = Some(cx.new(|_| {
            crate::features::context::ContextPanel::new(
                services.agents.clone(),
                services.executor.clone(),
            )
            .with_sessions(services.sessions.clone())
        }));
        let context_panel_event_subscription = context_panel.as_ref().map(|panel| {
            cx.subscribe_in(panel, window, |root, _panel, event, window, cx| {
                root.handle_context_panel_event(event.clone(), window, cx);
            })
        });
        let logs_view = services.commands.clone().map(|commands| {
            let executor = services.executor.clone();
            cx.new(|cx| crate::features::commands::LogsView::new(commands, executor, window, cx))
        });
        let mcp_runtime_panel = services.mcp_runtime.clone().map(|runtime| {
            let executor = services.executor.clone();
            cx.new(|_| crate::features::mcp_runtime::McpRuntimePanel::new(runtime, executor))
        });
        let updates_panel = cx.new(|_| {
            crate::features::updates::UpdatesPanel::with_notifications(
                services.update_service.clone(),
                services.executor.clone(),
                services.notifications.clone(),
            )
        });
        let updates_event_subscription =
            cx.subscribe_in(&updates_panel, window, |root, _panel, event, window, cx| {
                root.handle_updates_event(event, window, cx)
            });
        let quick_open_view = Some(cx.new(|cx| {
            crate::features::quick_open::QuickOpenView::new(services.clone(), window, cx)
        }));
        let quick_open_event_subscription = quick_open_view.as_ref().map(|view| {
            cx.subscribe_in(view, window, |root, _view, event, window, cx| {
                root.handle_quick_open_event(event, window, cx);
            })
        });
        let workspace_orphan_cleanup_panel =
            Some(cx.new(|_| workspace_orphan_cleanup::WorkspaceOrphanCleanupPanel::new()));
        let workspace_orphan_cleanup_subscription =
            workspace_orphan_cleanup_panel.as_ref().map(|panel| {
                cx.subscribe_in(panel, window, |root, _panel, event, _, cx| {
                    root.handle_workspace_orphan_cleanup_event(event.clone(), cx);
                })
            });
        let agent_terminal_tools = services.agents.as_ref().map(|_| {
            cx.new(|cx| crate::features::terminal_tools::TerminalToolsView::new(None, window, cx))
        });
        let agent_terminal_tools_event_subscription = agent_terminal_tools.as_ref().map(|tools| {
            cx.subscribe_in(tools, window, |root, tools, event, window, cx| {
                if !command_blocks::handle_event_from(root, tools.clone(), event, window, cx) {
                    root.handle_terminal_tools_event_from(tools.clone(), event, window, cx);
                }
                if let crate::features::terminal_tools::TerminalToolsEvent::CloseSearch {
                    session_id,
                } = event
                    && root.agent_terminal_session_id.as_ref() == Some(session_id)
                    && root.agent_terminal_lifecycle == Some(SessionLifecycle::Running)
                    && let Some(input) = root.pane_input_handlers.get(session_id)
                {
                    root.focus_agent_terminal(input.entity.clone(), window, cx);
                }
            })
        });
        let agent_terminal_tools_observer_subscription = agent_terminal_tools
            .as_ref()
            .map(|tools| cx.observe(tools, |_, _, cx| cx.notify()));
        let workspace_tabs_view = cx.new(|_| WorkspaceTabs::new(Vec::new()));
        let workspace_tabs_root = cx.weak_entity();
        workspace_tabs_view.update(cx, |tabs, cx| {
            tabs.set_host_navigation(host_navigation.clone(), cx);
            let root = workspace_tabs_root.clone();
            tabs.set_close_confirmation_focus_hook(
                Some(std::rc::Rc::new(
                    move |phase, target, request_id, window, tabs_cx| {
                        let _ = root.update(tabs_cx, |root, cx| {
                            root.handle_workspace_tab_close_confirmation_focus(
                                phase,
                                target.clone(),
                                request_id,
                                window,
                                cx,
                            );
                        });
                    },
                )),
                cx,
            );
        });
        let workspace_tabs_create_subscription = cx.subscribe_in(
            &workspace_tabs_view,
            window,
            |root, _tabs, event: &WorkspaceTabsCreateEvent, window, cx| {
                root.handle_workspace_tabs_create_event(event, window, cx);
            },
        );
        // 先由 Controller 确认激活意图；拒绝时不允许回调单独切换 Root 的可见 pane。
        let workspace_tabs_subscription = cx.subscribe_in(
            &workspace_tabs_view,
            window,
            |root, _tabs, event, window, cx| {
                let activation_accepted = root.handle_workspace_tab_event(event, window, cx);
                if activation_accepted && let WorkspaceTabEvent::Activate(tab_id) = event {
                    root.activate_workspace_tab(tab_id, window, cx);
                }
            },
        );
        let workspace_tabs_observer_subscription =
            cx.observe_in(&workspace_tabs_view, window, |_root, _tabs, _window, cx| {
                cx.notify()
            });
        let connections_event_subscription = connections_panel.as_ref().map(|panel| {
            cx.subscribe_in(
                panel,
                window,
                |root, _panel, event, window, cx| {
                    if !connections::handle_connections_event(root, event, window, cx) {
                        // 外部 target 携带 RAII lease；当前 Root 尚未提供 workspace consumer，
                        // 明确拒绝并释放本次 scope，避免将“未处理”伪装成已接入。
                        root.status = Some(
                            crate::locale::text(
                                "外部终端目标目前无法打开，临时连接已取消。",
                                "This external terminal target cannot be opened yet; its temporary connection was cancelled.",
                            )
                        );
                        cx.notify();
                    }
                },
            )
        });
        let connections_editor_observer = connections_panel.as_ref().map(|panel| {
            cx.observe_in(panel, window, |root, panel, window, cx| {
                let (editor_open, prompt_open) = {
                    let panel = panel.read(cx);
                    (panel.editor_open(), panel.root_confirmation_active())
                };
                root.sync_connection_editor_modal(editor_open, prompt_open, window, cx);
            })
        });
        let agent_event_subscription = agents_panel.as_ref().map(|panel| {
            cx.subscribe_in(
                panel,
                window,
                |root, _panel, event, window, cx| match event {
                    crate::features::agents::AgentPanelEvent::NewTerminalFocusStarted {
                        request_generation,
                    } => {
                        root.pending_tool_rail_placement_focus = None;
                        root.begin_agent_creation_focus_intent(*request_generation, window, cx);
                    }
                    crate::features::agents::AgentPanelEvent::NewTerminalFocusReady {
                        request_generation,
                        agent_session_id,
                        terminal_session_id,
                    } => {
                        root.complete_agent_creation_focus_intent(
                            *request_generation,
                            agent_session_id.clone(),
                            terminal_session_id.clone(),
                            window,
                            cx,
                        );
                    }
                    crate::features::agents::AgentPanelEvent::NewTerminalFocusCancelled {
                        request_generation,
                    } => {
                        root.cancel_agent_creation_focus_intent(*request_generation);
                    }
                    crate::features::agents::AgentPanelEvent::Selected {
                        agent_session_id,
                        terminal_session_id,
                    } => {
                        if root.host_shutdown_requested {
                            return;
                        }
                        root.retain_agent_creation_focus_for_selection(
                            agent_session_id.as_ref(),
                            terminal_session_id.as_ref(),
                        );
                        root.select_agent_terminal(
                            agent_session_id.clone(),
                            terminal_session_id.clone(),
                            window,
                            cx,
                        );
                        if let Some(panel) = root.mcp_runtime_panel.as_ref() {
                            panel.update(cx, |panel, cx| panel.refresh(cx));
                        }
                    }
                    crate::features::agents::AgentPanelEvent::OpenCustomAgentManager => {
                        root.open_custom_agent_manager(window, cx);
                    }
                    crate::features::agents::AgentPanelEvent::OpenAgentLauncherSettings => {
                        if root.host_shutdown_requested {
                            return;
                        }
                        root.open_preferences_modal(window, cx);
                        root.preferences_panel.update(cx, |panel, cx| {
                            panel.show_agent_launcher_settings(window, cx);
                        });
                    }
                    crate::features::agents::AgentPanelEvent::TargetBindingChanged => {}
                },
            )
        });
        let files_pane_subscriptions = files_pane
            .as_ref()
            .map(|pane| Self::subscribe_files_pane_events(pane, window, cx));
        let sftp_panel_subscriptions = sftp_panel
            .as_ref()
            .map(|pane| Self::subscribe_files_pane_events(pane, window, cx));
        let editor_event_subscription = file_editor_workspace.as_ref().map(|editor| {
            cx.subscribe_in(
                editor,
                window,
                |root, editor, event, window, cx| match event {
                    FileEditorWorkspaceEvent::QuitReady => {
                        let _ = window;
                        root.emit_quit_ready(cx);
                    }
                    FileEditorWorkspaceEvent::QuitCancelled => {
                        root.cancel_pending_update_install(cx);
                        root.pending_quit_intent = None;
                        root.quit_preparation_failure = None;
                    }
                    FileEditorWorkspaceEvent::WorkspaceProjectionChanged => {
                        let projection = editor.read(cx).workspace_projection();
                        root.push_editor_workspace_projection(projection, window, cx);
                    }
                    FileEditorWorkspaceEvent::WorkspaceTabCloseResolved {
                        operation_id,
                        endpoint,
                        closed,
                    } => root.handle_workspace_file_close_result(
                        operation_id.clone(),
                        endpoint.clone(),
                        *closed,
                        cx,
                    ),
                },
            )
        });
        let editor_confirmation_observer = file_editor_workspace.as_ref().map(|editor| {
            cx.observe_in(editor, window, |root, editor, window, cx| {
                let active = editor.read(cx).root_confirmation_active();
                root.sync_editor_root_confirmation(editor.clone(), active, window, cx);
            })
        });
        let terminal_activation_subscription =
            cx.observe_window_activation(window, |root, window, cx| {
                if !window.is_window_active() {
                    // 应用失活时提前停止组件拥有的拖动与边缘滚动，GPUI 不会替应用清理 active_drag。
                    root.cancel_workspace_tab_drag(window, cx);
                }
                root.cancel_unfocused_pane_compositions(window, cx);
                cx.notify();
            });
        let root_entity = cx.entity();
        window.on_window_should_close(cx, move |window, cx| {
            root_entity.update(cx, |root, cx| {
                root.handle_window_close_request(window, cx);
            });
            // 有托盘时隐藏窗口；否则沿统一退出门禁处理，绝不绕过脏状态保护。
            false
        });
        let initial_color_scheme = TerminalColorScheme::Kerminal;
        let initial_workspace_sessions = services.workspace_sessions.clone();
        let workspace_load_pending = initial_workspace_sessions.is_some();
        let workspace_executor = services.executor.clone();
        cx.spawn(async move |this, cx| {
            let Some(workspace_sessions) = initial_workspace_sessions else {
                return;
            };
            let workspace_result = workspace_executor
                .run(move |cancellation| async move { workspace_sessions.load(cancellation).await })
                .await;
            let _ = this.update(cx, |root, cx| {
                match workspace_result {
                    Ok(load) => {
                        let initial_install = root.workspace_load_pending;
                        root.install_workspace_load(load, cx);
                        root.workspace_load_pending = false;
                        root.request_startup_workspace_focus(initial_install, cx);
                    }
                    Err(error) => {
                        root.workspace_session_state = None;
                        root.workspace_session_error =
                            Some(error.public_message().to_owned().into());
                        root.workspace_load_pending = false;
                    }
                }
                cx.notify();
            });
        })
        .detach();
        let workspace_broadcast = Self::init_workspace_broadcast(window, cx);
        let mut root = Self {
            window_handle: window.window_handle(),
            services,
            tray_owner_installed: false,
            is_creating_session: false,
            status: None,
            status_toast_revision: 0,
            status_toast_observed: None,
            configuration_recovery,
            settings_snapshot: None,
            window_appearance: initial_appearance,
            session_commands: session_queue::SessionCommandQueues::default(),
            terminal_wheels: terminal_wheel::TerminalWheelState::default(),
            session_watch_cancellations: HashMap::new(),
            pane_input_handlers: HashMap::new(),
            terminal_view_bindings: HashMap::new(),
            pane_input_binding_requests: HashMap::new(),
            pane_input_binding_failures: HashSet::new(),
            pane_input_binding_request_generation: 0,
            pending_pane_focus_intent: None,
            pane_focus_intent_generation: 0,
            pending_tool_rail_placement_focus: None,
            tool_rail_rendered_compact: (false, None),
            broadcast_session_ids: None,
            broadcast_generation: 0,
            pending_broadcast_generation: None,
            workspace_broadcast,
            pending_key_commits: VecDeque::new(),
            key_commit_generation: 0,
            keybinding_runtime: keybindings::RootKeybindingRuntime::default(),
            keybinding_focus: keybinding_focus::KeybindingFocusTracker::default(),
            terminal_command_suggestions: command_suggestions::TerminalCommandSuggestionBridge::new(
            ),
            terminal_command_contexts: HashMap::new(),
            command_line_projection: command_line_projection::CommandLineProjection::default(),
            suggestion_menu: None,
            suggestion_menu_binding: None,
            _suggestion_menu_subscription: None,
            focus_handle,
            compact_drawer_focus_handle,
            compact_drawer_return_focus: None,
            compact_host_drawer_open: false,
            compact_tool_drawer_open: false,
            preferences_modal_focus_handle,
            preferences_modal_open: false,
            preferences_modal_return_focus: None,
            preferences_modal_generation: 0,
            connections_modal_focus_handle,
            connections_modal_open: false,
            connection_prompt_modal_open: false,
            connections_modal_return_focus: None,
            connections_modal_generation: 0,
            group_rename_return_surface: None,
            root_confirmation_owner: None,
            root_confirmation_operations_view: None,
            root_confirmation_file_pane: None,
            root_confirmation_editor: None,
            root_confirmation_return_focus: None,
            root_confirmation_generation: 0,
            snippet_editor_modal_focus_handle,
            snippet_editor_modal_open: false,
            snippet_editor_modal_return_focus: None,
            snippet_editor_modal_generation: 0,
            custom_agent_manager: None,
            custom_agent_manager_subscription: None,
            custom_agent_manager_return_focus: None,
            reported_focus_session: None,
            _focus_in_subscription: focus_in_subscription,
            _focus_out_subscription: focus_out_subscription,
            center_panel: CenterPanel::Terminal,
            theme_mode: initial_theme_mode,
            terminal_metrics: TerminalMetrics::from_settings(
                &TerminalSettings::default(),
                &window.text_system().all_font_names(),
            ),
            terminal_scrollback: usize::try_from(TerminalSettings::default().scrollback)
                .unwrap_or(5_000),
            files_pane,
            sftp_panel,
            preferences_panel,
            file_editor_workspace,
            _editor_confirmation_observer: editor_confirmation_observer,
            commands_view,
            _snippet_editor_visibility_subscription: snippet_editor_visibility_subscription,
            _snippet_terminal_fill_subscription: snippet_terminal_fill_subscription,
            connections_panel,
            operations_view,
            _operations_event_subscription: operations_event_subscription,
            proxy_panel,
            _proxy_panel_event_subscription: proxy_panel_event_subscription,
            proxy_runtime: proxy_runtime::ProxyRuntime::default(),
            initial_proxy_notice: None,
            container_exec_refresh_targets: HashMap::new(),
            agents_panel,
            agent_target_candidate: CachedAgentTargetCandidate(None),
            right_dock_collapsed: false,
            agent_terminal_tools,
            _agent_terminal_tools_event_subscription: agent_terminal_tools_event_subscription,
            _agent_terminal_tools_observer_subscription: agent_terminal_tools_observer_subscription,
            terminal_tools_runtime: terminal_tools_runtime::TerminalToolsRuntime::default(),
            command_block_runtime: command_blocks::CommandBlockRuntime::default(),
            selected_agent_session_id: None,
            agent_terminal_session_id: None,
            agent_terminal_snapshot: None,
            agent_terminal_size: None,
            agent_terminal_lifecycle: None,
            pending_agent_resizes: HashMap::new(),
            failed_session_resizes: HashMap::new(),
            focused_agent_terminal: false,
            pending_agent_focus_intent: None,
            agent_watch_generation: 0,
            agent_watch_cancellation: CancellationToken::new(),
            agent_needs_snapshot: false,
            selected_host_id: None,
            _files_pane_subscriptions: files_pane_subscriptions,
            _sftp_panel_subscriptions: sftp_panel_subscriptions,
            _editor_event_subscription: editor_event_subscription,
            _preferences_event_subscription: preferences_event_subscription,
            _connections_event_subscription: connections_event_subscription,
            _connections_editor_observer: connections_editor_observer,
            host_navigation,
            _host_navigation_event_subscription: host_navigation_event_subscription,
            split_target_picker,
            split_target_picker_generation: 0,
            _split_target_picker_event_subscription: split_target_picker_event_subscription,
            _agent_event_subscription: agent_event_subscription,
            updates_panel,
            quick_open_view,
            quick_open_modal_open: false,
            _quick_open_event_subscription: quick_open_event_subscription,
            mcp_runtime_panel,
            _updates_event_subscription: updates_event_subscription,
            workspace_session_state: None,
            workspace_session_error: None,
            workspace_controller: None,
            workspace_runtime: WorkspaceRuntime::default(),
            workspace_files: workspace_files::WorkspaceFileRuntime::default(),
            files_workspace_selection: workspace_files::FilesWorkspaceSelection::default(),
            sftp_workbench_tab_id: TabId::new(),
            sftp_workbench_tab_open: false,
            sftp_workbench_close_after_batch: None,
            surface_navigation_generation: 0,
            pending_sftp_navigation: None,
            latest_sftp_request_id: None,
            pending_sftp_transfer_navigation: None,
            pending_sftp_host_creation: None,
            workspace_orphan_cleanup_panel,
            _workspace_orphan_cleanup_subscription: workspace_orphan_cleanup_subscription,
            workspace_tabs_view,
            _workspace_tabs_subscription: workspace_tabs_subscription,
            _workspace_tabs_create_subscription: workspace_tabs_create_subscription,
            _workspace_tabs_observer_subscription: workspace_tabs_observer_subscription,
            workbench_sidebar_resizable_state,
            _workbench_sidebar_resize_subscription: workbench_sidebar_resize_subscription,
            workbench_sidebar_restore_width: None,
            workbench_dock_restore_width: None,
            workbench_sidebar_restore_pending: false,
            workbench_dock_restore_pending: false,
            workbench_layout_restore_generation: 0,
            workbench_layout_intent: None,
            workbench_layout_save_error: None,
            workbench_bottom_restore_height: None,
            workbench_bottom_restore_pending: false,
            host_sidebar_collapsed: false,
            workbench_dock_resizable_state,
            workbench_bottom_resizable_state,
            tool_rail_state: workbench::ToolRailPanelState::default(),
            context_panel,
            _context_panel_event_subscription: context_panel_event_subscription,
            logs_view,
            tool_rail_operations_views: BTreeMap::new(),
            _tool_rail_operations_subscriptions: BTreeMap::new(),
            tool_rail_proxy_panels: BTreeMap::new(),
            _tool_rail_proxy_panel_subscriptions: BTreeMap::new(),
            session_reconnect: session_reconnect::SessionReconnectController::default(),
            workspace_renders: HashMap::new(),
            workspace_render_subscriptions: HashMap::new(),
            workspace_tools: HashMap::new(),
            workspace_tool_subscriptions: HashMap::new(),
            terminal_link_discovery_caches: HashMap::new(),
            workspace_restore_generations: HashMap::new(),
            #[cfg(feature = "acceptance-metrics")]
            pending_fresh_terminal_probes: HashMap::new(),
            workspace_restore_cancellations: HashMap::new(),
            workspace_resource_operations: HashMap::new(),
            pending_close_tab_confirmation_focus: None,
            pending_close_batch_confirmation_focus: None,
            pending_close_batch_focus: None,
            workspace_load_pending,
            pending_external_activations: Vec::new(),
            pending_quit_intent: None,
            quit_preparation_failure: None,
            quit_ready_emitted: false,
            quit_ready_intent: None,
            host_lifecycle: RootHostLifecycle::Running,
            host_shutdown_requested: false,
            host_shutdown_view_shown: false,
            host_failure_message: None,
            application_task_snapshot: kerminal_application::executor::ApplicationTaskSnapshot {
                accepting: true,
                active_tasks: 0,
            },
            failed_quit_intent: None,
            update_install_guard: UpdateInstallGuard::default(),
            pending_update_install: None,
            terminal_color_scheme: initial_color_scheme,
            cursor_blink_enabled: TerminalSettings::default().cursor_blink,
            cursor_blink_phase_visible: true,
            pending_palette_update: None,
            palette_update_running: false,
            palette_update_generation: 0,
            _appearance_subscription: appearance_subscription,
            _terminal_activation_subscription: terminal_activation_subscription,
        };
        root.refresh_agent_target_candidate(cx);
        root.refresh_context_panel_projection(cx);
        root.refresh_proxy_panel(cx);
        root.refresh_workspace_orphan_cleanup(cx);
        root.observe_application_executor(cx);
        root
    }

    /// 订阅共享 executor 的关闭快照；它只撤下工作台，不启动或代替 host 的清理事务。
    ///
    /// @author kongweiguang
    fn observe_application_executor(&mut self, cx: &mut Context<Self>) {
        let mut updates = self.services.executor.subscribe_state();
        let initial = *updates.borrow();
        self.apply_application_task_snapshot(initial, cx);
        cx.spawn(async move |this, cx| {
            loop {
                if updates.changed().await.is_err() {
                    return;
                }
                let snapshot = *updates.borrow();
                if this
                    .update(cx, |root, cx| {
                        root.apply_application_task_snapshot(snapshot, cx)
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();
    }

    /// executor intake 一旦关闭就单向屏蔽工作台；失败与成功仍由原 host lifecycle 回执决定。
    ///
    /// @author kongweiguang
    fn apply_application_task_snapshot(
        &mut self,
        snapshot: kerminal_application::executor::ApplicationTaskSnapshot,
        cx: &mut Context<Self>,
    ) {
        self.application_task_snapshot = snapshot;
        if !snapshot.accepting {
            self.host_shutdown_requested = true;
            if self.host_lifecycle == RootHostLifecycle::Running {
                self.host_lifecycle = RootHostLifecycle::Stopping;
            }
        }
        cx.notify();
    }

    /// 从 controller 的活动终端布局与 Ready runtime pane 生成 Agent 首选目标候选；不从焦点外的 Tab 推断目标。
    /// 收尾后的资源记账不再触发 Agent 列表 reload，保留已有业务 owner 给宿主继续清理。
    ///
    /// @author kongweiguang
    fn refresh_agent_target_candidate(&mut self, cx: &mut Context<Self>) {
        if self.host_shutdown_requested {
            return;
        }
        let candidate = self.current_agent_target_candidate();
        if self.agent_target_candidate.0 == candidate {
            return;
        }
        self.agent_target_candidate = CachedAgentTargetCandidate(candidate.clone());
        if let Some(panel) = self.agents_panel.clone() {
            panel.update(cx, |panel, cx| panel.set_target_candidate(candidate, cx));
        }
    }

    /// 只投影活动 Terminal tab 中完整 Ready 的目标；相同 Pane/Tab 标题只去重显示，不改变绑定。
    fn current_agent_target_candidate(
        &self,
    ) -> Option<crate::features::agents::AgentTargetCandidate> {
        let controller = self.workspace_controller.as_ref()?;
        let snapshot = controller.snapshot();
        let tab_id = snapshot.active_tab_id.as_ref()?;
        let Some(WorkspaceSessionTab::Terminal { title, layout, .. }) =
            snapshot.terminal_tabs.iter().find(|tab| tab.id() == tab_id)
        else {
            return None;
        };
        let pane_id = snapshot
            .focused_pane_id
            .as_ref()
            .filter(|pane_id| layout.contains_pane(pane_id))
            .cloned()
            .or_else(|| layout.pane_ids().into_iter().next())?;
        let pane = self.workspace_runtime.pane(&pane_id)?;
        if pane.tab_id != *tab_id || pane.phase != RuntimePanePhase::Ready {
            return None;
        }
        let session_id = pane.session_id.as_ref()?;
        let target = pane.target.as_ref()?;
        if !pane
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| &snapshot.session_id == session_id)
        {
            return None;
        }

        let (target_kind, target_ref) = match target {
            SessionTarget::Local(_) => ("local", None),
            SessionTarget::Ssh(target) => ("ssh", Some(target.host_id.as_str().to_owned())),
            SessionTarget::Telnet(target) => {
                ("telnet", Some(format!("{}:{}", target.host, target.port)))
            }
            SessionTarget::Serial(_) => ("serial", None),
            SessionTarget::Container(target) => ("container", Some(target.container_id.clone())),
        };
        let tab_title = title.trim();
        let pane_title = pane.persisted.title.trim();
        let label = if pane_title.is_empty() || pane_title == tab_title {
            title.clone()
        } else {
            format!("{tab_title} · {pane_title}")
        };
        Some(crate::features::agents::AgentTargetCandidate {
            binding: AgentTargetBinding {
                binding_id: None,
                generation: 0,
                pane_id: Some(pane_id),
                tab_id: Some(tab_id.clone()),
                terminal_session_id: Some(session_id.clone()),
                target_ref,
                target_kind: Some(target_kind.to_owned()),
                cwd: None,
                shell: None,
                live_status: AgentTargetLiveStatus::Ready,
                last_seen_at: None,
            },
            label,
        })
    }

    /// 记录组合根确认的托盘 owner；没有真实 owner 时，窗口关闭必须经过退出门禁。
    pub fn set_tray_owner_installed(&mut self, installed: bool, _cx: &mut Context<Self>) {
        self.tray_owner_installed = installed;
    }

    /// 组合根只能回投已脱敏的运行态提示；正文不作为业务状态机输入。
    pub fn report_host_status(
        &mut self,
        safe_message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.status = Some(safe_message.into());
        cx.notify();
    }

    /// 把首次启动和单实例转发的 activation 送进同一应用用例，并合并尚未完成的完全相同请求。
    ///
    /// @author kongweiguang
    pub fn queue_external_activation(
        &mut self,
        activation: ExternalActivation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.host_lifecycle != RootHostLifecycle::Running {
            self.status = Some(crate::locale::text(
                "应用正在停止，无法接收新的外部连接。",
                "The application is stopping and cannot accept another external connection.",
            ));
            cx.notify();
            return false;
        }
        if self.pending_external_activations.contains(&activation) {
            return true;
        }
        let Some(service) = self.services.external_launch.clone() else {
            self.status = Some(crate::locale::text(
                "外部启动服务未配置，未打开连接。",
                "External launch is unavailable; no connection was opened.",
            ));
            cx.notify();
            return false;
        };
        let Some(panel) = self.connections_panel.clone() else {
            self.status = Some(crate::locale::text(
                "主机连接面板不可用，未打开外部连接。",
                "The connection panel is unavailable; no external connection was opened.",
            ));
            cx.notify();
            return false;
        };

        self.pending_external_activations.push(activation.clone());
        self.set_center_panel(CenterPanel::Connections, window, cx);
        self.status = Some(crate::locale::text(
            "正在检查外部连接请求…",
            "Checking the external connection request…",
        ));
        let executor = self.services.executor.clone();
        let activation_for_task = activation.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled("外部启动请求已取消"));
                    }
                    service.handle_activation(activation_for_task).await
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                root.pending_external_activations
                    .retain(|pending| pending != &activation);
                if root.host_lifecycle != RootHostLifecycle::Running {
                    return;
                }
                match result {
                    Ok(ExternalLaunchOutcome::Ignored) => {
                        root.status = None;
                    }
                    Ok(ExternalLaunchOutcome::UsernameRequired { request }) => {
                        panel.update(cx, |panel, cx| panel.request_external_username(request, cx));
                        root.status = None;
                    }
                    Ok(ExternalLaunchOutcome::PasswordRequired { target })
                    | Ok(ExternalLaunchOutcome::Ready { target }) => {
                        panel.update(cx, |panel, cx| panel.request_external_target(target, cx));
                        root.status = None;
                    }
                    Err(error) => {
                        root.status = Some(error.public_message().to_owned().into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        true
    }

    /// 按最新配置串行重配会话，并只刷新仍可见搜索的视图，避免主题改变遗留旧 revision 坐标。
    ///
    /// @author kongweiguang
    fn queue_terminal_palette_update(&mut self, cx: &mut Context<Self>) {
        self.palette_update_generation = self.palette_update_generation.wrapping_add(1).max(1);
        self.pending_palette_update = Some((
            self.current_terminal_palette(),
            self.terminal_scrollback,
            self.palette_update_generation,
        ));
        if self.palette_update_running {
            return;
        }
        self.palette_update_running = true;

        let sessions = self.services.sessions.clone();
        let executor = self.services.executor.clone();
        cx.spawn(async move |this, cx| {
            loop {
                let (palette, scrollback, generation) = match this.update(cx, |root, _| {
                    let Some(configuration) = root.pending_palette_update.take() else {
                        root.palette_update_running = false;
                        return None;
                    };
                    Some(configuration)
                }) {
                    Ok(Some(palette)) => palette,
                    Ok(None) | Err(_) => return,
                };

                let sessions_for_task = sessions.clone();
                let result = executor
                    .run(move |cancellation| async move {
                        let sessions = sessions_for_task.list(SessionFilter::default()).await?;
                        let mut updated_sessions = Vec::new();
                        let mut failures = Vec::new();
                        for session in sessions {
                            if cancellation.is_cancelled() {
                                return Err(AppError::cancelled("终端配色更新已取消"));
                            }
                            let session_id = session.session_id;
                            if let Err(error) = sessions_for_task
                                .set_palette(session_id.clone(), palette)
                                .await
                            {
                                failures.push(error.public_message().to_owned());
                                continue;
                            }
                            // 调色板已推进绘制 revision；后续历史容量更新失败也必须刷新搜索坐标。
                            updated_sessions.push(session_id.clone());
                            if let Err(error) = sessions_for_task
                                .set_scrollback_limit(session_id, scrollback)
                                .await
                            {
                                failures.push(error.public_message().to_owned());
                            }
                        }
                        Ok((updated_sessions, failures))
                    })
                    .await;

                let (updated_sessions, status) = match result {
                    Ok((updated_sessions, failures)) if failures.is_empty() => {
                        (updated_sessions, None)
                    }
                    Ok((updated_sessions, failures)) => (
                        updated_sessions,
                        Some(format!("部分终端未能应用新配色：{}", failures[0])),
                    ),
                    Err(error) => (Vec::new(), Some(error.public_message().to_owned())),
                };
                let _ = this.update(cx, |root, cx| {
                    if root.palette_update_generation != generation {
                        return;
                    }
                    for session_id in updated_sessions {
                        root.refresh_open_terminal_search_for_view_revision(&session_id, cx);
                    }
                    if let Some(status) = status {
                        root.status = Some(status.into());
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 按窗口外观和用户设置取得此次渲染、OSC 响应都要使用的 engine 调色板。
    fn current_terminal_palette(&self) -> TerminalPalette {
        terminal_palette_for_mode(
            self.window_appearance,
            self.theme_mode,
            self.terminal_color_scheme,
        )
    }

    /// 完整快照仍应用其它偏好；有未确认布局时只更新持久基线，不回滚更晚的窗口手势。
    ///
    /// @author kongweiguang
    fn apply_settings_snapshot(
        &mut self,
        settings: SettingsSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let layout = settings.workbench_layout.clone();
        let restore_layout = workbench::should_restore_workbench_layout(
            self.settings_snapshot
                .as_ref()
                .map(|settings| &settings.workbench_layout),
            &layout,
            self.workbench_layout_intent.is_some(),
        );
        let show_tab_numbers = settings.terminal.show_tab_numbers;
        let confirm_terminal_close = settings.terminal.confirm_close_tab;
        let suggestion_settings = settings.terminal.inline_suggestion.clone();
        self.keybinding_runtime.update(&settings.keybindings);
        self.terminal_command_suggestions
            .set_settings(Some(suggestion_settings));
        preferences::apply_snapshot(self, settings, window, cx);
        self.refresh_active_suggestion_context(cx);
        self.workspace_tabs_view.update(cx, |tabs, cx| {
            tabs.set_show_tab_numbers(show_tab_numbers, cx);
            tabs.set_confirm_terminal_close(confirm_terminal_close, cx);
        });
        self.remap_tool_rail_settings(cx);
        if restore_layout {
            self.restore_workbench_layout(&layout, cx);
        }
        self.refresh_tool_rail_panel_theme(cx);
        self.refresh_context_panel_projection(cx);
        // 引擎具备 OSC 133 投影能力；子视图仍要求当前会话提供真实命令块才展示轨道。
        let command_block_rail_visible = preferences::interaction_policy(self)
            .is_some_and(|policy| policy.command_block_rail_visible(true));
        let tools = self
            .workspace_tools
            .values()
            .cloned()
            .chain(self.agent_terminal_tools.clone())
            .collect::<Vec<_>>();
        for tools in tools {
            tools.update(cx, |tools, cx| {
                tools.set_command_block_rail_visible(command_block_rail_visible, cx);
            });
        }
    }

    /// 按已加载的关键词规则刷新工具投影；规则编译失败时保留上一份有效匹配器。
    fn refresh_terminal_keyword_rules(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        preferences::refresh_keyword_rules(self, window, cx);
    }

    /// 为广播目标校验创建新代次，并撤销旧代次尚未发送的输入。
    ///
    /// @author kongweiguang
    pub(super) fn begin_broadcast_start(&mut self) -> u64 {
        let (generation, dropped) = self.session_commands.begin_broadcast_start();
        self.broadcast_generation = generation;
        self.pending_broadcast_generation = Some(generation);
        self.broadcast_session_ids = None;
        if dropped.broadcast_events > 0 {
            self.status = Some(crate::locale::text(
                format!("已取消 {} 条旧广播输入。", dropped.broadcast_events),
                format!(
                    "Cancelled {} queued inputs from the previous broadcast.",
                    dropped.broadcast_events
                ),
            ));
        }
        generation
    }

    /// 只接受仍然最新的广播验证回执，避免 Stop 或新一轮 Start 被迟到结果覆盖。
    pub(super) fn broadcast_start_is_current(&self, generation: u64) -> bool {
        self.broadcast_generation == generation
            && self.pending_broadcast_generation == Some(generation)
            && self.session_commands.broadcast_start_is_current(generation)
    }

    /// 验证通过后固定目标的 UI owner/generation；队列与输入 CAS 共用这一份不可变绑定。
    pub(super) fn activate_broadcast(
        &mut self,
        generation: u64,
        targets: Vec<UiSessionBinding>,
    ) -> bool {
        if !self.broadcast_start_is_current(generation) || targets.is_empty() {
            return false;
        }
        if !self
            .session_commands
            .activate_broadcast(generation, targets.clone())
        {
            return false;
        }
        self.pending_broadcast_generation = None;
        self.broadcast_session_ids = Some(targets);
        true
    }

    /// 停止路由并撤销待发广播；仅 Root 子模块共享此入口，不向 crate 暴露队列内部统计类型。
    /// 正在进行的一次写入允许完成，避免把停止操作误报为已撤销 transport 写入。
    ///
    /// @author kongweiguang
    fn stop_broadcast(&mut self) -> session_queue::QueueDropSummary {
        self.broadcast_generation = self.broadcast_generation.wrapping_add(1).max(1);
        self.pending_broadcast_generation = None;
        self.broadcast_session_ids = None;
        let dropped = self.session_commands.stop_broadcast();
        self.on_root_broadcast_stopped();
        dropped
    }

    /// 返回该 session 当前运行时持有的快照，不从焦点或旧标签镜像推断。
    fn snapshot_for_session(&self, session_id: &SessionId) -> Option<&RenderSnapshot> {
        self.workspace_runtime
            .pane_for_session(session_id)
            .and_then(|pane_id| self.workspace_runtime.pane(pane_id))
            .and_then(|pane| pane.snapshot.as_ref())
            .or_else(|| {
                (self.agent_terminal_session_id.as_ref() == Some(session_id))
                    .then_some(self.agent_terminal_snapshot.as_ref())
                    .flatten()
            })
    }

    /// 接收文件浏览器的显式 endpoint 并打开统一编辑器，不从当前终端或选中主机猜目标。
    pub fn open_file_endpoint(
        &mut self,
        endpoint: FileEndpoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.file_editor_workspace.as_ref() else {
            self.status = Some("文件编辑服务尚未配置".into());
            cx.notify();
            return;
        };
        editor.update(cx, |workspace, cx| workspace.open_file(endpoint, cx));
        self.set_center_panel(CenterPanel::Editor, window, cx);
    }

    /// 显式折叠只持久化折叠意图；临时抽屉不走此入口，尺寸仍来自先前用户拖动而非窄窗测量。
    ///
    /// @author kongweiguang
    pub(super) fn toggle_host_sidebar(
        &mut self,
        _event: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_host_sidebar_collapsed(!self.host_sidebar_collapsed, cx);
    }

    /// 标题栏和主机栏共用显式折叠入口；重复收起不反向展开，也不丢失用户先前拖动宽度。
    ///
    /// @author kongweiguang
    pub(super) fn set_host_sidebar_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        if self.host_sidebar_collapsed == collapsed {
            return;
        }
        let was_collapsed = self.host_sidebar_collapsed;
        if !was_collapsed {
            let sidebar_width = {
                let state = self.workbench_sidebar_resizable_state.read(cx);
                state
                    .preferred_panel_size(0)
                    .or_else(|| state.sizes().first().copied())
            };
            self.workbench_sidebar_restore_width = sidebar_width;
        }
        if !self.right_dock_collapsed && !self.workbench_dock_restore_pending {
            let dock_width = {
                let state = self.workbench_dock_resizable_state.read(cx);
                state
                    .preferred_panel_size(1)
                    .or_else(|| state.sizes().get(1).copied())
            };
            self.workbench_dock_restore_width = dock_width;
        }

        self.host_sidebar_collapsed = collapsed;
        self.workbench_sidebar_restore_pending =
            !self.host_sidebar_collapsed && self.workbench_sidebar_restore_width.is_some();
        self.workbench_dock_restore_pending = self.workbench_dock_restore_width.is_some();
        self.workbench_layout_restore_generation =
            self.workbench_layout_restore_generation.wrapping_add(1);
        let mut layout = self.current_workbench_layout_intent();
        layout.host_sidebar_collapsed = self.host_sidebar_collapsed;
        self.request_workbench_layout_intent(layout, cx);
        cx.notify();
    }

    /// 切换正文只清理旧焦点；Files 仅在共享来源已选中时投影临时工作台，独立传输保留自己的 TabId。
    /// 模态草稿打开时保留底层页面，不重定向左栏主机或终端工具目标。
    ///
    /// @author kongweiguang
    fn set_center_panel(
        &mut self,
        panel: CenterPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.group_rename_return_surface = None;
        // Preferences is a modal surface: preserving center_panel keeps its Entity mounted and
        // prevents settings navigation from resetting terminal focus, tabs, or running sessions.
        if panel == CenterPanel::Preferences {
            self.open_preferences_modal(window, cx);
            return;
        }
        if self.preferences_modal_open || self.connections_modal_open {
            return;
        }
        self.surface_navigation_generation = self.surface_navigation_generation.wrapping_add(1);
        if panel == CenterPanel::Files
            && self.files_workspace_selection == workspace_files::FilesWorkspaceSelection::Workbench
        {
            self.sftp_workbench_tab_open = true;
        }
        if panel != CenterPanel::Terminal || self.center_panel != panel {
            self.clear_pending_pane_focus();
        }
        if self.center_panel != panel {
            self.clear_keybinding_chord();
            self.pending_key_commits.clear();
            self.set_reported_focus_session(None, cx);
        }
        if panel != CenterPanel::Agents {
            self.focused_agent_terminal = false;
        }
        self.center_panel = panel;
        self.refresh_commands_session(cx);
        self.refresh_workspace_views(cx);
        if panel == CenterPanel::Terminal {
            if let Some(tab_id) = self
                .workspace_controller
                .as_ref()
                .and_then(|controller| controller.snapshot().active_tab_id.clone())
            {
                let pane_id = self.workspace_controller.as_ref().and_then(|controller| {
                    let snapshot = controller.snapshot();
                    let active = snapshot
                        .terminal_tabs
                        .iter()
                        .find(|tab| tab.id() == &tab_id)?;
                    let layout = active.terminal_layout()?;
                    snapshot
                        .focused_pane_id
                        .as_ref()
                        .filter(|pane_id| layout.contains_pane(pane_id))
                        .cloned()
                        .or_else(|| layout.pane_ids().into_iter().next())
                });
                if let Some(pane_id) = pane_id {
                    self.request_workspace_pane_focus(tab_id.clone(), pane_id.clone(), window, cx);
                    self.request_workspace_pane_snapshot(tab_id, pane_id, cx);
                } else {
                    self.clear_pending_pane_focus();
                    self.set_reported_focus_session(None, cx);
                }
            } else {
                self.clear_pending_pane_focus();
                self.set_reported_focus_session(None, cx);
                // 空工作区没有 pane 子焦点；将快捷键焦点留在 Workspace scope，而不抢占编辑器或现存终端。
                window.focus(&self.focus_handle, cx);
            }
        } else if panel == CenterPanel::Agents {
            let session_id = self.agent_terminal_session_id.clone();
            let input_handler = session_id.as_ref().and_then(|session_id| {
                let agent_session_id = self.selected_agent_session_id.clone()?;
                self.ensure_pane_input_handler(
                    session_id.clone(),
                    SessionOwner::Agent { agent_session_id },
                    cx,
                )
            });
            if let Some(input_handler) = input_handler {
                let focus_handle = input_handler.read(cx).focus_handle();
                self.focused_agent_terminal = true;
                window.focus(&focus_handle, cx);
            }
            self.set_reported_focus_session(session_id, cx);
            self.request_agent_snapshot(cx);
        }
        if panel == CenterPanel::Terminal {
            self.request_visible_workspace_snapshots(cx);
        }
        self.refresh_proxy_panel(cx);
        self.refresh_active_suggestion_context(cx);
        self.cancel_unfocused_pane_compositions(window, cx);
        cx.notify();
    }

    /// 临时工作台只有实际显示共享 Files pane 时才占用顶栏选中态。
    ///
    /// @author kongweiguang
    pub(super) fn shared_sftp_workbench_is_selected(&self) -> bool {
        self.sftp_workbench_tab_open
            && self.center_panel == CenterPanel::Files
            && self.files_workspace_selection == workspace_files::FilesWorkspaceSelection::Workbench
    }

    /// 共享文件工作台入口显式切换展示来源；独立 SFTP 传输标签仍由其持久 TabId 单独激活。
    ///
    /// @author kongweiguang
    pub(super) fn open_shared_sftp_workbench(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.files_pane.is_none() {
            self.status = Some(crate::locale::text(
                "文件服务尚未配置，无法打开 SFTP 工作台。",
                "File services are unavailable, so the SFTP workbench cannot be opened.",
            ));
            cx.notify();
            return false;
        }
        if self.preferences_modal_open || self.connections_modal_open {
            return false;
        }
        // 显式重新进入工作台会撤销旧批次的延迟关闭，避免后续无关 ACK 收起刚打开的视图。
        self.sftp_workbench_close_after_batch = None;
        self.files_workspace_selection = workspace_files::FilesWorkspaceSelection::Workbench;
        self.sftp_workbench_tab_open = true;
        self.clear_pending_pane_focus();
        self.set_reported_focus_session(None, cx);
        self.set_center_panel(CenterPanel::Files, window, cx);
        true
    }

    /// 顶栏快捷键和关闭确认共用可见 Tab 身份，避免共享工作台借用 controller 的后台 active。
    ///
    /// @author kongweiguang
    pub(super) fn visible_workspace_tab_id(&self) -> Option<TabId> {
        if self.shared_sftp_workbench_is_selected() {
            return Some(self.sftp_workbench_tab_id.clone());
        }
        self.workspace_controller
            .as_ref()?
            .snapshot()
            .active_tab_id
            .clone()
    }

    /// 让设置面板常驻于 RootView，保存与关闭确认状态不会因切换终端而丢失。
    fn open_preferences(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.set_center_panel(CenterPanel::Preferences, window, cx);
    }

    /// 打开设置时撤销旧 pane 聚焦回执，再建立独立焦点边界；关闭后回到同一 Root 句柄也不能复活旧意图。
    ///
    /// @author kongweiguang
    fn open_preferences_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preferences_modal_open || self.connections_modal_open {
            return;
        }
        self.clear_pending_pane_focus();
        self.preferences_modal_return_focus = window.focused(cx);
        self.preferences_modal_open = true;
        self.preferences_modal_generation = self.preferences_modal_generation.wrapping_add(1);
        self.clear_keybinding_chord();
        self.pending_key_commits.clear();
        let generation = self.preferences_modal_generation;
        let focus_handle = self.preferences_modal_focus_handle.clone();
        window.focus(&focus_handle, cx);
        self.cancel_unfocused_pane_compositions(window, cx);
        cx.notify();

        let root_entity = cx.entity();
        let preferences_panel = self.preferences_panel.clone();
        window.on_next_frame(move |window, app| {
            root_entity.update(app, |root, cx| {
                if !root.preferences_modal_open || root.preferences_modal_generation != generation {
                    return;
                }
                preferences_panel.update(cx, |panel, cx| {
                    panel.focus_navigation_search(window, cx);
                });
            });
        });
    }

    /// 通过原 dirty-close gate 后可 flush 独立布局队列；撤下弹层不销毁 owner，也不把未落盘布局视作成功。
    /// 延迟恢复时让其它模态、Quick Open、退出流程和系统提示保有焦点；管理器恢复只接受真实版本化读回。
    ///
    /// @author kongweiguang
    fn close_preferences_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.preferences_modal_open {
            return;
        }
        self.preferences_panel
            .update(cx, |panel, cx| panel.flush_pending_workbench_layout(cx));
        self.preferences_modal_open = false;
        self.preferences_modal_generation = self.preferences_modal_generation.wrapping_add(1);
        let return_focus = self.preferences_modal_return_focus.take();
        let generation = self.preferences_modal_generation;
        let root_entity = cx.entity();
        cx.notify();
        window.on_next_frame(move |window, app| {
            root_entity.update(app, |root, cx| {
                if root.preferences_modal_generation != generation || window.has_active_prompt() {
                    return;
                }
                if let Some(manager) = root.custom_agent_manager.clone() {
                    let focus = manager.read(cx).focus_handle(cx);
                    window.focus(&focus, cx);
                    manager.update(cx, |manager, cx| manager.request_readback_from_root(cx));
                    return;
                }
                if root.connection_prompt_modal_open {
                    root.focus_connection_prompt(window, cx);
                    return;
                }
                if root.root_confirmation_owner.is_some() {
                    root.focus_root_confirmation(window, cx);
                    return;
                }
                if terminal_input_modal_blocked(
                    logical_input_modal_blocked(
                        root.preferences_modal_open,
                        root.connections_modal_open,
                        root.snippet_editor_modal_open || root.custom_agent_manager.is_some(),
                        false,
                        root.pending_quit_intent.is_some(),
                    ),
                    root.quick_open_modal_open,
                ) {
                    return;
                }
                if let Some(focus_handle) = return_focus {
                    if root.focus_handle.contains(&focus_handle, window) {
                        window.focus(&focus_handle, cx);
                    } else {
                        window.focus(&root.focus_handle, cx);
                    }
                } else {
                    window.focus(&root.focus_handle, cx);
                }
                root.cancel_unfocused_pane_compositions(window, cx);
            });
        });
    }

    /// 在同一 Connections Entity 内打开主机草稿；焦点先离开终端，失败或取消都保留原中心页。
    ///
    /// @author kongweiguang
    pub(super) fn prepare_connection_editor_modal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(panel) = self.connections_panel.clone() else {
            return false;
        };
        if self.preferences_modal_open {
            return false;
        }
        if self.connections_modal_open {
            return false;
        }

        // 模态曾经打开即失效旧异步焦点请求，不能仅依赖最终句柄是否相同。
        self.clear_pending_pane_focus();
        self.connections_modal_return_focus = window.focused(cx);
        self.connections_modal_open = true;
        self.connections_modal_generation = self.connections_modal_generation.wrapping_add(1);
        self.clear_keybinding_chord();
        self.pending_key_commits.clear();
        let generation = self.connections_modal_generation;
        let focus_handle = self.connections_modal_focus_handle.clone();
        window.focus(&focus_handle, cx);
        self.cancel_unfocused_pane_compositions(window, cx);
        cx.notify();

        let root_entity = cx.entity();
        window.on_next_frame(move |window, app| {
            root_entity.update(app, |root, cx| {
                if !root.connections_modal_open
                    || root.connection_prompt_modal_open
                    || root.connections_modal_generation != generation
                {
                    return;
                }
                if panel.read(cx).editor_open() {
                    panel.update(cx, |panel, cx| {
                        panel.focus_editor_primary_input(window, cx);
                    });
                } else {
                    root.close_connection_editor_modal(window, cx);
                }
            });
        });
        true
    }

    /// 跟随 ConnectionsPanel 的草稿状态开关遮罩，直接由实体观察器调用而不轮询。
    /// 保存后的 SFTP 回投等真实模态关闭再继续，不以若干渲染帧猜测表单何时收起。
    ///
    /// @author kongweiguang
    fn sync_connection_editor_modal(
        &mut self,
        editor_open: bool,
        prompt_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt_was_open = self.connection_prompt_modal_open;
        self.connection_prompt_modal_open = prompt_open;
        if editor_open {
            let _ = self.prepare_connection_editor_modal(window, cx);
            if prompt_open && !prompt_was_open {
                self.focus_connection_prompt(window, cx);
            } else if !prompt_open && prompt_was_open {
                self.focus_connection_editor(window, cx);
            }
        } else if prompt_open {
            let modal_was_open = self.connections_modal_open;
            let _ = self.prepare_connection_confirmation_modal(window, cx);
            if modal_was_open && !prompt_was_open {
                self.focus_connection_prompt(window, cx);
            }
        } else {
            self.close_connection_editor_modal(window, cx);
            self.resume_sftp_host_creation(window, cx);
        }
    }

    /// 危险确认固定发起它的 Entity；其它运维面板刷新不能关闭或替换当前目标。
    ///
    /// @author kongweiguang
    fn sync_operations_root_confirmation(
        &mut self,
        operations: Entity<crate::features::operations::OperationsView>,
        active: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if active {
            if self.root_confirmation_owner.is_none()
                || self.root_confirmation_owner == Some(RootConfirmationOwner::Operations)
                    && self
                        .root_confirmation_operations_view
                        .as_ref()
                        .is_some_and(|current| current.entity_id() == operations.entity_id())
            {
                self.root_confirmation_operations_view = Some(operations);
                self.open_root_confirmation(RootConfirmationOwner::Operations, None, window, cx);
            }
        } else if self.root_confirmation_owner == Some(RootConfirmationOwner::Operations)
            && self
                .root_confirmation_operations_view
                .as_ref()
                .is_some_and(|current| current.entity_id() == operations.entity_id())
        {
            self.close_root_confirmation(RootConfirmationOwner::Operations, window, cx);
        }
    }

    /// 编辑器确认固定发起 Entity；切标签或恢复文件不能把原确认转向另一份文件实体。
    ///
    /// @author kongweiguang
    fn sync_editor_root_confirmation(
        &mut self,
        editor: Entity<FileEditorWorkspace>,
        active: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let owns_current = self.root_confirmation_owner == Some(RootConfirmationOwner::Editor)
            && self
                .root_confirmation_editor
                .as_ref()
                .is_some_and(|current| current.entity_id() == editor.entity_id());
        if active {
            if self.root_confirmation_owner.is_none() || owns_current {
                self.root_confirmation_editor = Some(editor);
                self.open_root_confirmation(RootConfirmationOwner::Editor, None, window, cx);
                // 同一 Entity 可从关闭确认推进到覆盖或下一份退出确认，必须重绘而不重置焦点 owner。
                cx.notify();
            }
        } else if owns_current {
            self.close_root_confirmation(RootConfirmationOwner::Editor, window, cx);
        }
    }

    /// 被其它根确认暂挡的请求仍保留在原 feature；空闲后只提升一项，不重放业务操作。
    ///
    /// @author kongweiguang
    fn sync_pending_root_confirmation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.root_confirmation_owner.is_some() {
            return;
        }
        let operations = self
            .operations_view
            .iter()
            .chain(self.tool_rail_operations_views.values())
            .find(|view| view.read(cx).root_confirmation_active())
            .cloned();
        if let Some(operations) = operations {
            self.sync_operations_root_confirmation(operations, true, window, cx);
            return;
        }
        let files = self
            .files_pane
            .iter()
            .chain(self.sftp_panel.iter())
            .cloned()
            .chain(self.workspace_files.files_panes())
            .find(|pane| pane.read(cx).pending_delete_confirmation().is_some());
        if let Some(pane) = files {
            self.sync_file_delete_root_confirmation(pane, true, window, cx);
            return;
        }
        if let Some(editor) = self.file_editor_workspace.clone()
            && editor.read(cx).root_confirmation_active()
        {
            self.sync_editor_root_confirmation(editor, true, window, cx);
        }
    }

    /// 文件删除的目标 Entity 固定在发起确认的 pane；其它文件标签的刷新不能改写本次目标。
    ///
    /// @author kongweiguang
    pub(super) fn sync_file_delete_root_confirmation(
        &mut self,
        pane: Entity<crate::features::files::FilesPane>,
        active: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if active {
            if self.root_confirmation_owner.is_none()
                || self.root_confirmation_owner == Some(RootConfirmationOwner::FileDelete)
                    && self
                        .root_confirmation_file_pane
                        .as_ref()
                        .is_some_and(|current| current.entity_id() == pane.entity_id())
            {
                self.open_root_confirmation(
                    RootConfirmationOwner::FileDelete,
                    Some(pane),
                    window,
                    cx,
                );
            }
        } else if self.root_confirmation_owner == Some(RootConfirmationOwner::FileDelete)
            && self
                .root_confirmation_file_pane
                .as_ref()
                .is_some_and(|current| current.entity_id() == pane.entity_id())
        {
            self.close_root_confirmation(RootConfirmationOwner::FileDelete, window, cx);
        }
    }

    /// 新确认开始时撤销旧 pane 聚焦回执并冻结原焦点，避免关闭确认后旧回执抢焦点或按键穿透。
    ///
    /// @author kongweiguang
    fn open_root_confirmation(
        &mut self,
        owner: RootConfirmationOwner,
        file_pane: Option<Entity<crate::features::files::FilesPane>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.root_confirmation_owner == Some(owner) {
            if file_pane.is_some() {
                self.root_confirmation_file_pane = file_pane;
            }
            return;
        }
        if self.root_confirmation_owner.is_some() {
            return;
        }
        self.clear_pending_pane_focus();
        self.root_confirmation_return_focus = window.focused(cx);
        self.root_confirmation_owner = Some(owner);
        self.root_confirmation_file_pane = file_pane;
        self.root_confirmation_generation = self.root_confirmation_generation.wrapping_add(1);
        self.clear_keybinding_chord();
        self.pending_key_commits.clear();
        self.cancel_unfocused_pane_compositions(window, cx);
        cx.notify();
        self.schedule_root_confirmation_focus(window, cx);
    }

    /// 关闭只发生在 feature 已消费/撤销待确认状态后；异步恢复焦点受代次和其它模态保护。
    ///
    /// @author kongweiguang
    fn close_root_confirmation(
        &mut self,
        owner: RootConfirmationOwner,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.root_confirmation_owner != Some(owner) {
            return;
        }
        self.root_confirmation_owner = None;
        self.root_confirmation_operations_view = None;
        self.root_confirmation_file_pane = None;
        self.root_confirmation_editor = None;
        self.root_confirmation_generation = self.root_confirmation_generation.wrapping_add(1);
        let generation = self.root_confirmation_generation;
        let return_focus = self.root_confirmation_return_focus.take();
        let root_entity = cx.entity();
        cx.notify();
        window.on_next_frame(move |window, app| {
            root_entity.update(app, |root, cx| {
                if root.root_confirmation_generation != generation || window.has_active_prompt() {
                    return;
                }
                if root.root_confirmation_owner.is_some() {
                    root.focus_root_confirmation(window, cx);
                    return;
                }
                if root.connection_prompt_modal_open {
                    root.focus_connection_prompt(window, cx);
                    return;
                }
                if root.preferences_modal_open {
                    window.focus(&root.preferences_modal_focus_handle, cx);
                    return;
                }
                if root.connections_modal_open {
                    root.focus_connection_editor(window, cx);
                    return;
                }
                if root.snippet_editor_modal_open {
                    window.focus(&root.snippet_editor_modal_focus_handle, cx);
                    return;
                }
                if root.pending_quit_intent.is_some() {
                    return;
                }
                if let Some(focus) = return_focus {
                    if root.focus_handle.contains(&focus, window) {
                        window.focus(&focus, cx);
                    } else {
                        window.focus(&root.focus_handle, cx);
                    }
                } else {
                    window.focus(&root.focus_handle, cx);
                }
            });
        });
    }

    /// 延迟到 overlay 已挂载后聚焦；连接认证提示仍具有更高优先级。
    ///
    /// @author kongweiguang
    fn schedule_root_confirmation_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let owner = self.root_confirmation_owner;
        let generation = self.root_confirmation_generation;
        let root_entity = cx.entity();
        window.on_next_frame(move |window, app| {
            root_entity.update(app, |root, cx| {
                if root.root_confirmation_owner != owner
                    || root.root_confirmation_generation != generation
                    || root.connection_prompt_modal_open
                {
                    return;
                }
                root.focus_root_confirmation(window, cx);
            });
        });
    }

    /// 删除、编辑和运维共用根 trap；仅运维需要再把焦点交给其内部确认按钮组。
    ///
    /// @author kongweiguang
    fn focus_root_confirmation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connection_prompt_modal_open {
            return;
        }
        window.focus(&self.connections_modal_focus_handle, cx);
        if self.root_confirmation_owner == Some(RootConfirmationOwner::Operations)
            && let Some(operations) = self.root_confirmation_operations_view.clone()
        {
            operations.update(cx, |view, cx| view.focus_root_confirmation(window, cx));
        }
    }

    /// Escape/遮罩取消只调用 feature 的撤销入口；正在消费或提交的危险操作不作伪取消。
    ///
    /// @author kongweiguang
    fn cancel_root_confirmation(&mut self, cx: &mut Context<Self>) {
        match self.root_confirmation_owner {
            Some(RootConfirmationOwner::Operations) => {
                if let Some(operations) = self.root_confirmation_operations_view.clone() {
                    operations.update(cx, |view, cx| view.cancel_root_confirmation(cx));
                }
            }
            Some(RootConfirmationOwner::FileDelete) => {
                if let Some(pane) = self.root_confirmation_file_pane.clone() {
                    pane.update(cx, |pane, cx| pane.cancel_delete_confirmation(cx));
                }
            }
            Some(RootConfirmationOwner::Editor) => {
                if let Some(editor) = self.root_confirmation_editor.clone() {
                    editor.update(cx, |editor, cx| editor.cancel_root_confirmation(cx));
                }
            }
            None => {}
        }
    }

    /// 单独出现的连接确认也提升为 Root 模态，避免它只遮住左栏而让工作区仍可操作。
    ///
    /// @author kongweiguang
    fn prepare_connection_confirmation_modal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.connections_modal_open {
            self.clear_pending_pane_focus();
            self.connections_modal_return_focus = window.focused(cx);
            self.connections_modal_open = true;
            self.connections_modal_generation = self.connections_modal_generation.wrapping_add(1);
            self.clear_keybinding_chord();
            self.pending_key_commits.clear();
            cx.notify();
            self.focus_connection_prompt(window, cx);
        }
        true
    }

    /// 先聚焦 Root 的 trap，再下一帧把输入移交给一次性认证框；信任确认本身保留 trap 焦点。
    ///
    /// @author kongweiguang
    fn focus_connection_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.connections_panel.clone() else {
            return;
        };
        let focus_handle = self.connections_modal_focus_handle.clone();
        window.focus(&focus_handle, cx);
        let generation = self.connections_modal_generation;
        let root_entity = cx.entity();
        window.on_next_frame(move |window, app| {
            root_entity.update(app, |root, cx| {
                if !root.connection_prompt_modal_open
                    || !root.connections_modal_open
                    || root.connections_modal_generation != generation
                {
                    return;
                }
                panel.update(cx, |panel, cx| {
                    panel.focus_root_confirmation(window, cx);
                });
            });
        });
        self.cancel_unfocused_pane_compositions(window, cx);
    }

    /// 确认提示关闭但编辑器仍开时恢复表单输入，不让焦点落回被遮罩的终端。
    ///
    /// @author kongweiguang
    fn focus_connection_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.connections_panel.clone() else {
            return;
        };
        let generation = self.connections_modal_generation;
        let root_entity = cx.entity();
        window.on_next_frame(move |window, app| {
            root_entity.update(app, |root, cx| {
                if !root.connections_modal_open
                    || root.connection_prompt_modal_open
                    || root.connections_modal_generation != generation
                {
                    return;
                }
                if root.root_confirmation_owner.is_some() {
                    root.focus_root_confirmation(window, cx);
                    return;
                }
                panel.update(cx, |panel, cx| panel.focus_editor_primary_input(window, cx));
            });
        });
    }

    /// 遮罩、Escape 和取消按钮共用 ConnectionsPanel 的保存/脏草稿门禁。
    ///
    /// @author kongweiguang
    pub(super) fn request_close_connection_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.connections_modal_open || self.connection_prompt_modal_open {
            return;
        }
        if let Some(panel) = self.connections_panel.clone() {
            panel.update(cx, |panel, cx| panel.request_editor_close(window, cx));
        } else {
            self.close_connection_editor_modal(window, cx);
        }
    }

    /// 只有编辑器已通过自身关闭门禁后才撤下模态；延迟恢复时让其它模态、Quick Open、退出流程和系统提示保有焦点。
    ///
    /// @author kongweiguang
    fn close_connection_editor_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.connections_modal_open {
            return;
        }
        self.connections_modal_open = false;
        self.connections_modal_generation = self.connections_modal_generation.wrapping_add(1);
        let return_focus = self.connections_modal_return_focus.take();
        let generation = self.connections_modal_generation;
        let root_entity = cx.entity();
        cx.notify();
        window.on_next_frame(move |window, app| {
            root_entity.update(app, |root, cx| {
                if root.connections_modal_generation != generation || window.has_active_prompt() {
                    return;
                }
                if root.root_confirmation_owner.is_some() {
                    root.focus_root_confirmation(window, cx);
                    return;
                }
                if root.preferences_modal_open {
                    if let Some(focus_handle) = return_focus.as_ref() {
                        window.focus(focus_handle, cx);
                    }
                    return;
                }
                if terminal_input_modal_blocked(
                    logical_input_modal_blocked(
                        root.preferences_modal_open,
                        root.connections_modal_open,
                        root.snippet_editor_modal_open || root.custom_agent_manager.is_some(),
                        false,
                        root.pending_quit_intent.is_some(),
                    ),
                    root.quick_open_modal_open,
                ) {
                    return;
                }
                if let Some(focus_handle) = return_focus {
                    if root.focus_handle.contains(&focus_handle, window) {
                        window.focus(&focus_handle, cx);
                    } else {
                        window.focus(&root.focus_handle, cx);
                    }
                } else {
                    window.focus(&root.focus_handle, cx);
                }
                root.cancel_unfocused_pane_compositions(window, cx);
            });
        });
    }

    /// 设置遮罩的关闭请求只委托面板脏状态守卫，不直接撤下 Root modal 状态。
    ///
    /// @author kongweiguang
    pub(super) fn request_close_preferences_modal(&mut self, cx: &mut Context<Self>) {
        if !self.preferences_modal_open {
            return;
        }
        self.preferences_panel
            .update(cx, |panel, cx| panel.request_close(cx));
    }

    /// 搜索导航和左栏入口共用同一 MCP 面板 Entity，只刷新快照并切换中心面板。
    ///
    /// @author kongweiguang
    fn show_mcp_runtime(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.mcp_runtime_panel.as_ref() {
            panel.update(cx, |panel, cx| panel.refresh(cx));
            self.set_center_panel(CenterPanel::McpRuntime, window, cx);
        }
    }

    /// 仅在用户明确新建终端时调用创建用例，并在首快照前订阅合并通知。
    fn create_local_terminal(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.create_terminal_for_target(TerminalTargetRequest::ProfileDefaultLocal, window, cx);
    }

    /// 已加载目录名称只用于初始标签；默认 Profile 未就绪或缺失时回退，启动目标仍由既有用例解析。
    /// 退出阶段在预留 pane 前拒绝新建，避免恢复用例早退后遗留无法完成的创建状态。
    /// 异步主机预检可能晚于另一创建请求返回，因此统一在此反馈忙状态，避免菜单请求静默丢失。
    ///
    /// @author kongweiguang
    fn create_terminal_for_target(
        &mut self,
        target_request: TerminalTargetRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.host_shutdown_requested
            || self.pending_quit_intent.is_some()
            || self.quit_ready_intent.is_some()
        {
            return;
        }
        if self.workspace_load_pending {
            self.status = Some("正在读取工作区，请稍后再创建终端。".into());
            cx.notify();
            return;
        }
        if self.is_creating_session {
            self.status = Some(crate::locale::text(
                "正在创建终端，请稍后重试。",
                "A terminal is being created. Try again when it finishes.",
            ));
            cx.notify();
            return;
        }
        self.is_creating_session = true;
        self.status = Some(
            match &target_request {
                TerminalTargetRequest::ProfileDefaultLocal => "正在启动本地终端…",
                TerminalTargetRequest::ProfileLocal { .. } => "正在启动本地 Profile…",
                TerminalTargetRequest::ExplicitHost { .. } => "正在连接终端…",
            }
            .into(),
        );

        let recovery_reserved_height = recovery_banner_height(&self.configuration_recovery);
        let size =
            terminal_size_for_window(window, recovery_reserved_height, &self.terminal_metrics);
        let host_id = match &target_request {
            TerminalTargetRequest::ExplicitHost { host_id, .. } => Some(host_id),
            _ => None,
        };
        let cached_host_display_name = match &target_request {
            TerminalTargetRequest::ProfileLocal { profile_id } => self
                .host_navigation
                .as_ref()
                .and_then(|navigation| navigation.read(cx).display_profile_name_for(profile_id)),
            TerminalTargetRequest::ProfileDefaultLocal => self
                .host_navigation
                .as_ref()
                .and_then(|navigation| navigation.read(cx).display_default_profile_name()),
            _ => host_id.and_then(|host_id| {
                self.host_navigation
                    .as_ref()
                    .and_then(|navigation| navigation.read(cx).display_name_for(host_id))
            }),
        };
        let tab_title = terminal_tab_title(&target_request, cached_host_display_name);
        let (target, pane, transient) = match target_request {
            TerminalTargetRequest::ProfileDefaultLocal => {
                let (pane, transient) = workspace_pane_for_terminal_target(None, &tab_title, None);
                (None, pane, transient)
            }
            TerminalTargetRequest::ProfileLocal { profile_id } => {
                let (mut pane, _) = workspace_pane_for_terminal_target(None, &tab_title, None);
                pane.profile_id = Some(profile_id.clone());
                pane.target = Some(WorkspaceTargetRef::Local {
                    profile_id: Some(profile_id),
                });
                (None, pane, false)
            }
            TerminalTargetRequest::ExplicitHost { host_id, target } => {
                let (pane, transient) =
                    workspace_pane_for_terminal_target(Some(&target), &tab_title, Some(&host_id));
                (Some(target), pane, transient)
            }
        };
        let reservation = match self.reserve_new_workspace_terminal_tab(
            pane,
            tab_title.to_string(),
            size,
            transient,
            cx,
        ) {
            Ok(reservation) => reservation,
            Err(error) => {
                self.is_creating_session = false;
                self.status = Some(error);
                cx.notify();
                return;
            }
        };
        let pane_id = reservation.pane_id.clone();
        self.set_center_panel(CenterPanel::Terminal, window, cx);
        self.remember_new_workspace_pane_focus_intent(
            reservation.tab_id.clone(),
            pane_id.clone(),
            reservation.generation,
            window,
            cx,
        );
        if self
            .workspace_runtime
            .set_pending_size(&pane_id, size)
            .is_err()
        {
            self.clear_pending_pane_focus();
            reservation.cancellation.cancel();
            self.cancel_workspace_restore(&pane_id);
            let _ = self.workspace_runtime.remove_empty_pane(&pane_id);
            self.workspace_runtime
                .remove_tab_layout(&reservation.tab_id);
            self.is_creating_session = false;
            self.status = Some("无法为终端 pane 保留窗口尺寸。".into());
            self.refresh_workspace_views(cx);
            cx.notify();
            return;
        }

        #[cfg(feature = "acceptance-metrics")]
        if target
            .as_ref()
            .is_none_or(|target| matches!(target, SessionTarget::Local(_)))
        {
            self.pending_fresh_terminal_probes
                .insert(pane_id.clone(), reservation.generation);
        }

        if let Some(target) = target {
            if let Err(error) = self.open_workspace_terminal_pane(
                terminal_open::WorkspaceTerminalOpenRequest {
                    tab_id: reservation.tab_id.clone(),
                    pane_id: pane_id.clone(),
                    kind: WorkspaceTerminalOpenKind::Direct(target),
                    size,
                    lease: None,
                    transient: reservation.transient,
                    generation: reservation.generation,
                    cancellation: reservation.cancellation.clone(),
                },
                cx,
            ) {
                self.clear_pending_pane_focus();
                reservation.cancellation.cancel();
                self.cancel_workspace_restore(&pane_id);
                let _ = self.workspace_runtime.remove_empty_pane(&pane_id);
                self.workspace_runtime
                    .remove_tab_layout(&reservation.tab_id);
                self.is_creating_session = false;
                self.status = Some(error);
                self.refresh_workspace_views(cx);
                cx.notify();
            }
        } else {
            // Profile 默认目标只能由统一 resolver 读取；不会在 UI 中猜测或复制 profile 配置。
            self.restore_workspace_pane(reservation.tab_id, reservation.pane, cx);
        }
    }
    /// 模态及主机弹层阻断底层快捷键，但放行已聚焦搜索框的原生字符，避免 Windows 丢失 WM_CHAR。
    ///
    /// @author kongweiguang
    fn handle_workbench_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root_modal_open = terminal_input_modal_blocked(
            self.preferences_modal_open
                || self.connections_modal_open
                || self.connection_prompt_modal_open
                || self.root_confirmation_owner.is_some()
                || self.snippet_editor_modal_open
                || self.custom_agent_manager.is_some()
                || self.split_target_picker.read(cx).is_open()
                || self
                    .workspace_tabs_view
                    .read(cx)
                    .is_create_target_picker_open(),
            self.quick_open_modal_open,
        );
        if root_modal_open
            && keybinding_focus::modal_key_uses_platform_text(&window.context_stack(), event)
        {
            return;
        }
        if self.split_target_picker.read(cx).is_open()
            || self
                .workspace_tabs_view
                .read(cx)
                .is_create_target_picker_open()
        {
            cx.stop_propagation();
            return;
        }
        if self.quick_open_modal_open {
            // 焦点意外落回工作区时也只截断下层按键；已聚焦 Quick Open 文本输入已在上方交还平台。
            cx.stop_propagation();
            return;
        }
        if self.connection_prompt_modal_open {
            if event.keystroke.key.eq_ignore_ascii_case("escape")
                && let Some(panel) = self.connections_panel.clone()
            {
                panel.update(cx, |panel, cx| panel.cancel_root_confirmation(window, cx));
            }
            cx.stop_propagation();
            return;
        }
        if self.root_confirmation_owner.is_some() {
            if event.keystroke.key.eq_ignore_ascii_case("escape") {
                self.cancel_root_confirmation(cx);
            }
            // Buttons and their focus trap own Tab/Enter; Root blocks application chords and pane input.
            cx.stop_propagation();
            return;
        }
        if self.snippet_editor_modal_open {
            if event.keystroke.key.eq_ignore_ascii_case("escape") {
                snippet_editor::handle_escape(self, cx);
            }
            cx.stop_propagation();
            return;
        }
        if self.custom_agent_manager.is_some()
            && !self.preferences_modal_open
            && !self.connections_modal_open
        {
            if event.keystroke.key.eq_ignore_ascii_case("escape") {
                self.request_close_custom_agent_manager(cx);
            }
            cx.stop_propagation();
            return;
        }
        if self.preferences_modal_open || self.connections_modal_open {
            let key = event.keystroke.key.to_ascii_lowercase();
            if key == "escape" {
                if self.preferences_modal_open {
                    let panel = self.preferences_panel.clone();
                    panel.update(cx, |panel, cx| panel.handle_escape(window, cx));
                } else {
                    self.request_close_connection_editor(window, cx);
                }
            } else if self.preferences_modal_open
                && key == "f"
                && event.keystroke.modifiers.secondary()
            {
                let panel = self.preferences_panel.clone();
                panel.update(cx, |panel, cx| panel.focus_navigation_search(window, cx));
            }
            // 子输入框先处理编辑按键；Root 到达时统一阻止快捷键、Tab 切换和终端输入穿透。
            cx.stop_propagation();
            return;
        }
        if event.keystroke.key.eq_ignore_ascii_case("escape")
            && let Some((preview, _, _)) = self.pending_workspace_split_preview(cx)
            && self
                .workspace_resource_operations
                .get(&preview.pane_id)
                .is_some_and(|entry| {
                    entry.operation_id() == preview.operation_id
                        && !entry.cancel_requested()
                        && !entry.split_measurement_started()
                })
            && self.cancel_unadmitted_workspace_split(&preview, cx)
        {
            cx.stop_propagation();
            return;
        }
        self.observe_keybinding_focus(window, cx);
        if self.focus_handle.is_focused(window)
            && let Some(binding) = self.logical_focused_terminal_view_binding(cx)
            && !self.pane_input_handlers.contains_key(&binding.session_id)
            && self.handle_terminal_history_key_down(binding, event, window, cx)
        {
            return;
        }
        let contexts = window.context_stack();
        let is_modal = terminal_input_modal_blocked(
            window.has_active_prompt()
                || self.pending_quit_intent.is_some()
                || self.preferences_modal_open
                || self.connections_modal_open
                || self.connection_prompt_modal_open
                || self.root_confirmation_owner.is_some()
                || self.snippet_editor_modal_open
                || self.custom_agent_manager.is_some()
                || self.split_target_picker.read(cx).is_open()
                || self
                    .workspace_tabs_view
                    .read(cx)
                    .is_create_target_picker_open(),
            self.quick_open_modal_open,
        );
        let focus_scope = keybinding_focus::classify_focus_scope(&contexts, is_modal);
        let ime_composing = focus_scope == keybindings::KeybindingFocusScope::Terminal
            && self.pane_input_handlers.values().any(|registration| {
                let input = registration.entity.read(cx);
                input.focus_handle().is_focused(window) && input.is_composing()
            });
        let text_input_focused = keybinding_focus::is_text_input_context(&contexts);
        app_keybindings::handle_app_key_down(
            self,
            event,
            focus_scope,
            ime_composing,
            text_input_focused,
            window,
            cx,
        );
    }

    /// 同一焦点身份承接 chord；焦点句柄变化时立即丢弃旧前缀，避免跨控件拼接按键。
    /// 观测真实焦点变化时同时撤销未提交 IME，但不触碰已排入原 SessionId FIFO 的输入。
    ///
    /// @author kongweiguang
    fn observe_keybinding_focus(&mut self, window: &Window, cx: &mut Context<Self>) {
        let focused = window.focused(cx);
        if self.keybinding_focus.observe(focused.clone()) {
            self.keybinding_runtime.clear_pending();
        }
        if self
            .pending_pane_focus_intent
            .as_ref()
            .is_some_and(|intent| focused.as_ref() != intent.expected_focus.as_ref())
        {
            self.clear_pending_pane_focus();
        }
        self.cancel_unfocused_pane_compositions(window, cx);
    }

    /// Tab/中心面板显式切换会使 chord 身份失效，即使异步焦点迁移尚未完成。
    fn clear_keybinding_chord(&mut self) {
        self.keybinding_focus.reset();
        self.keybinding_runtime.clear_pending();
    }

    /// Composition invalidates command-line guesses and lets the platform route physical keys.
    fn suspend_suggestions_for_ime(&mut self, binding: &UiSessionBinding, cx: &mut Context<Self>) {
        self.keybinding_runtime.clear_pending();
        if self.suggestion_menu_binding.as_ref() == Some(binding)
            || self.is_active_suggestion_binding(binding)
        {
            self.terminal_command_suggestions.clear();
            self.suggestion_menu_binding = None;
            self.sync_suggestion_menu(cx);
        }
    }

    /// 将确实会被当前终端协议编码的 key-up 投递给原输入 Entity；空释放事件不能触发 FIFO 的回底语义。
    /// 用已接受快照的模式判定，保留 Kitty `REPORT_EVENT_TYPES` 的真实释放通知。
    ///
    /// @author kongweiguang
    fn handle_pane_key_up(
        &mut self,
        input_handler: Entity<PaneTerminalInputHandler>,
        event: &KeyUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let binding = input_handler.read(cx).binding().clone();
        if !self
            .pane_input_handlers
            .get(&binding.session_id)
            .is_some_and(|registered| registered.binding == binding)
        {
            return;
        }
        if input_handler.read(cx).is_composing() {
            self.suspend_suggestions_for_ime(&binding, cx);
            cx.stop_propagation();
            return;
        }
        let Some(key) = terminal_key(&event.keystroke.key) else {
            return;
        };
        let modes = self
            .terminal_snapshot_for_binding(&binding)
            .map(|snapshot| snapshot.modes)
            .unwrap_or_default();
        let Some(input) = terminal_keyboard::terminal_key_release_input(
            key,
            self.terminal_input_modifiers(event.keystroke.modifiers),
            modes,
        ) else {
            cx.stop_propagation();
            return;
        };
        let _ = input_handler.update(cx, |handler, cx| handler.emit_input_event(input, cx));
        cx.stop_propagation();
    }

    /// 在 macOS 上依旧设置决定 Option/Alt 是终端 Meta 还是普通文本修饰键。
    fn terminal_input_modifiers(&self, modifiers: gpui_kit::Modifiers) -> InputModifiers {
        #[cfg(target_os = "macos")]
        let mut modifiers = input_modifiers(modifiers);
        #[cfg(not(target_os = "macos"))]
        let modifiers = input_modifiers(modifiers);
        #[cfg(target_os = "macos")]
        {
            let option_is_meta = preferences::interaction_policy(self)
                .map(|policy| policy.mac_option_is_meta())
                .unwrap_or_else(|| TerminalSettings::default().mac_option_is_meta);
            if !option_is_meta {
                modifiers.alt = false;
            }
        }
        modifiers
    }

    /// Agent PTY 在右栏内取得独立焦点；不改变中央 workspace 当前 tab 或 pane。
    ///
    /// @author kongweiguang
    fn focus_agent_terminal(
        &mut self,
        input_handler: Entity<PaneTerminalInputHandler>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_pending_pane_focus();
        let binding = input_handler.read(cx).binding().clone();
        let SessionOwner::Agent { agent_session_id } = &binding.owner else {
            return;
        };
        let session_id = binding.session_id.clone();
        if self.agent_terminal_session_id.as_ref() != Some(&session_id)
            || self.selected_agent_session_id.as_ref() != Some(agent_session_id)
            || !self
                .pane_input_handlers
                .get(&session_id)
                .is_some_and(|registered| registered.binding == binding)
        {
            return;
        }
        self.focused_agent_terminal = true;
        let focus_handle = input_handler.read(cx).focus_handle();
        window.focus(&focus_handle, cx);
        self.set_reported_focus_session(Some(session_id.clone()), cx);
        self.cancel_unfocused_pane_compositions(window, cx);
        self.refresh_commands_session(cx);
        self.request_agent_snapshot(cx);
    }

    /// 仅对焦点实际迁移的 session 排入 DECFocusReporting 事件，避免工具切换重复报告旧 Tab。
    /// 路由依据真实 owner 而非活动 Tab；关闭中或已退役的旧 ID 只清本地记录，不再补发 blur。
    /// 焦点通知使用固定 binding 的 UI FIFO，不得被 raw 输入路径误记成外部命令行编辑。
    ///
    /// @author kongweiguang
    fn set_reported_focus_session(
        &mut self,
        session_id: Option<SessionId>,
        cx: &mut Context<Self>,
    ) {
        let session_id = session_id.filter(|session_id| {
            self.pane_input_handlers
                .get(session_id)
                .is_some_and(|handler| {
                    self.session_command_owner(session_id).as_ref() == Some(&handler.binding.owner)
                })
        });
        if self.reported_focus_session == session_id {
            return;
        }
        if let Some(previous) = self.reported_focus_session.take()
            && self.session_command_owner(&previous).is_some()
        {
            self.enqueue_bound_focus_report(&previous, false, cx);
        }
        if let Some(session_id) = session_id
            && self.enqueue_bound_focus_report(&session_id, true, cx)
        {
            self.reported_focus_session = Some(session_id);
        }
    }

    /// 复用输入队列与 owner/generation 校验；通知不签正文票据，也不假装一次用户编辑。
    ///
    /// @author kongweiguang
    fn enqueue_bound_focus_report(
        &mut self,
        session_id: &SessionId,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(registration) = self.pane_input_handlers.get(session_id) else {
            return false;
        };
        let binding = registration.binding.clone();
        if self.session_command_owner(session_id).as_ref() != Some(&binding.owner) {
            return false;
        }
        self.enqueue_session_command(
            SessionCommand::UiInput(Box::new(session_queue::UiInputWork::new(
                UiTerminalInputRequest {
                    binding,
                    expected_context: None,
                    projection: UiInputProjection::Tracked,
                    event: TerminalInputEvent::Focus(focused),
                },
                None,
            ))),
            cx,
        )
    }

    /// 暂存同一次 Kitty keydown 的关联文本，仅短暂等待 GPUI 后续可能发出的重复 commit。
    ///
    /// @author kongweiguang
    fn remember_key_commit(&mut self, session_id: SessionId, text: String, cx: &mut Context<Self>) {
        self.key_commit_generation = self.key_commit_generation.wrapping_add(1).max(1);
        let generation = self.key_commit_generation;
        if self.pending_key_commits.len() >= 32 {
            self.pending_key_commits.pop_front();
        }
        self.pending_key_commits.push_back(PendingKeyCommit {
            session_id,
            text,
            generation,
        });
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(KEY_TEXT_COMMIT_WINDOW).await;
            let _ = this.update(cx, |view, _| {
                view.pending_key_commits
                    .retain(|pending| pending.generation > generation);
            });
        })
        .detach();
    }

    /// 仅吞掉同 session 且字节完全相同的键盘文本回调，组合输入与其他 commit 仍正常送入终端。
    ///
    /// @author kongweiguang
    fn consume_duplicate_key_commit(&mut self, session_id: &SessionId, text: &str) -> bool {
        let Some(index) = self
            .pending_key_commits
            .iter()
            .position(|pending| &pending.session_id == session_id && pending.text == text)
        else {
            return false;
        };
        self.pending_key_commits.remove(index);
        true
    }

    /// 只把真实 Canvas 网格放入 pane FIFO；运行态尺寸要等 PTY ACK 才能提交。
    /// 同一失败尺寸等待用户调整布局或重新绑定后再尝试，避免失败通知引起无界重绘重试。
    ///
    /// @author kongweiguang
    pub(crate) fn resize_workspace_pane_from_canvas(
        &mut self,
        pane_id: PaneId,
        size: TerminalSize,
        cx: &mut Context<Self>,
    ) {
        let Some(session_id) = self.workspace_runtime.session_for_pane(&pane_id).cloned() else {
            return;
        };
        if !session_command_pump::allow_canvas_resize_after_failure(
            &mut self.failed_session_resizes,
            &session_id,
            size,
        ) {
            return;
        }
        let needs_resize = self
            .workspace_runtime
            .resize_needs_enqueue(&pane_id, size)
            .unwrap_or(false);
        if !needs_resize {
            return;
        }
        self.enqueue_session_command(SessionCommand::Resize { session_id, size }, cx);
    }

    /// Agent 的 Canvas 也以 PTY ACK 为准；失败尺寸不由重绘自动重发，换尺寸或换代才解除。
    ///
    /// @author kongweiguang
    pub(crate) fn resize_agent_terminal_from_canvas(
        &mut self,
        session_id: SessionId,
        size: TerminalSize,
        cx: &mut Context<Self>,
    ) {
        if self.agent_terminal_lifecycle != Some(SessionLifecycle::Running)
            || self.agent_terminal_session_id.as_ref() != Some(&session_id)
        {
            return;
        }
        if !session_command_pump::allow_canvas_resize_after_failure(
            &mut self.failed_session_resizes,
            &session_id,
            size,
        ) || self.agent_terminal_size == Some(size)
            || self.pending_agent_resizes.get(&session_id) == Some(&size)
        {
            return;
        }
        self.enqueue_session_command(SessionCommand::Resize { session_id, size }, cx);
    }

    /// 偏好只需触发重绘；真正的行列数必须由每个可见 Canvas 的 bounds 计算。
    pub(super) fn resize_active_terminal(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        cx.notify();
    }

    /// 从 controller 的 active tab 与 focused pane 解析 session，避免依赖旧标签列表。
    fn active_session_id(&self) -> Option<SessionId> {
        let controller = self.workspace_controller.as_ref()?;
        let snapshot = controller.snapshot();
        let tab_id = snapshot.active_tab_id.as_ref()?;
        let tab = snapshot
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == tab_id)?;
        let layout = tab.terminal_layout()?;
        let pane_id = snapshot
            .focused_pane_id
            .as_ref()
            .filter(|pane_id| layout.contains_pane(pane_id))
            .cloned()
            .or_else(|| layout.pane_ids().into_iter().next())?;
        self.workspace_runtime.session_for_pane(&pane_id).cloned()
    }

    /// 键盘只发往明确获得焦点的 Agent 或中央 workspace，切换面板不会隐式换目标。
    fn focused_terminal_session_id(&self) -> Option<SessionId> {
        if self.focused_agent_terminal {
            return self.agent_terminal_session_id.clone();
        }
        (self.center_panel == CenterPanel::Terminal)
            .then(|| self.active_session_id())
            .flatten()
    }

    /// 只把活动 Ready pane 已安装的完整 UI binding 暴露给命令面板，并附上同代展示名。
    ///
    /// 活动 pane 尚未 Ready、已断开或尚无通过 terminal context 校验的 input handler 时，清空目标；
    /// 不从侧栏、历史连接或 Agent 首选目标推测替代 session。会话 ID 未变时仍刷新 binding 和名称。
    ///
    /// @author kongweiguang
    fn refresh_commands_session(&mut self, cx: &mut Context<Self>) {
        let (binding, presentation) = self.current_commands_target(cx);
        let session_id = binding.as_ref().map(|binding| binding.session_id.clone());
        if let Some(commands) = self.commands_view.as_ref() {
            commands.update(cx, |view, cx| {
                view.set_session(session_id, cx);
                view.set_session_binding(binding, cx);
                view.set_target_presentation(presentation, cx);
            });
        }
    }

    /// 工作区 tab 改名只重算名称投影，不重新载入 Commands session 或打断当前编辑状态。
    ///
    /// @author kongweiguang
    fn refresh_commands_target_presentation(&self, cx: &mut Context<Self>) {
        let (_, presentation) = self.current_commands_target(cx);
        if let Some(commands) = self.commands_view.as_ref() {
            commands.update(cx, |view, cx| {
                view.set_target_presentation(presentation, cx);
            });
        }
    }

    /// 一次快照同时解析活动 binding 与展示标签，避免标签和真实 session 来自不同 pane。
    ///
    /// @author kongweiguang
    fn current_commands_target(
        &self,
        cx: &Context<Self>,
    ) -> (
        Option<UiSessionBinding>,
        Option<crate::features::commands::CommandsTargetPresentation>,
    ) {
        let Some(controller) = self.workspace_controller.as_ref() else {
            return (None, None);
        };
        let snapshot = controller.snapshot();
        let binding = Self::resolve_active_commands_binding(
            snapshot,
            &self.workspace_runtime,
            |session_id| {
                self.pane_input_handlers
                    .get(session_id)
                    .map(|registration| &registration.binding)
            },
        );
        let presentation = binding.as_ref().and_then(|binding| {
            Self::resolve_commands_target_presentation(binding, snapshot, |pane_id| {
                self.workspace_pane_title(pane_id, cx).to_string()
            })
        });
        (binding, presentation)
    }

    /// 按活动工作区快照解析唯一可用的 UI pane；handler binding 是 generation 的权威来源。
    ///
    /// 复用命令日志对 pane/tab/owner 的匹配约束，但不读取目标主机或焦点组件状态，
    /// 使工具面板打开、窗口焦点移动等 UI 行为不会隐式切换会话。
    ///
    /// @author kongweiguang
    fn resolve_active_commands_binding<'a>(
        snapshot: &kerminal_application::workspace::WorkspaceSessionSnapshot,
        runtime: &WorkspaceRuntime,
        mut binding_for_session: impl FnMut(&SessionId) -> Option<&'a UiSessionBinding>,
    ) -> Option<UiSessionBinding> {
        let tab_id = snapshot.active_tab_id.as_ref()?;
        let tab = snapshot
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == tab_id)?;
        let layout = tab.terminal_layout()?;
        let pane_id = snapshot
            .focused_pane_id
            .as_ref()
            .filter(|pane_id| layout.contains_pane(pane_id))
            .cloned()
            .or_else(|| layout.pane_ids().into_iter().next())?;
        let pane = runtime.pane(&pane_id)?;
        if pane.tab_id != *tab_id || pane.phase != RuntimePanePhase::Ready {
            return None;
        }
        let session_id = pane.session_id.as_ref()?;
        let binding = binding_for_session(session_id)?;
        if binding.session_id != *session_id
            || binding.owner
                != (SessionOwner::Ui {
                    tab_id: tab_id.clone(),
                    pane_id,
                })
        {
            return None;
        }
        Some(binding.clone())
    }

    /// 目标标签从同一 tab、pane 的可信 UI 投影派生；缺少任何一项时不编造身份。
    ///
    /// @author kongweiguang
    fn resolve_commands_target_presentation(
        binding: &UiSessionBinding,
        snapshot: &WorkspaceSessionSnapshot,
        mut pane_label_for: impl FnMut(&PaneId) -> String,
    ) -> Option<crate::features::commands::CommandsTargetPresentation> {
        let SessionOwner::Ui { tab_id, pane_id } = &binding.owner else {
            return None;
        };
        if snapshot.active_tab_id.as_ref() != Some(tab_id) {
            return None;
        }
        let tab = snapshot
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == tab_id)?;
        if !tab
            .terminal_layout()
            .is_some_and(|layout| layout.contains_pane(pane_id))
        {
            return None;
        }
        let label = merge_commands_target_labels(tab.title(), &pane_label_for(pane_id))?;
        Some(crate::features::commands::CommandsTargetPresentation {
            binding: binding.clone(),
            label,
        })
    }

    /// 历史仅投影当前明确聚焦 pane 的已验证 binding；失去焦点目标时显示空态，不能猜首个 pane 或侧栏主机。
    ///
    /// @author kongweiguang
    pub(super) fn current_logs_target(
        &self,
        _cx: &Context<Self>,
    ) -> Option<crate::features::commands::LogsTarget> {
        let controller = self.workspace_controller.as_ref()?;
        let snapshot = controller.snapshot();
        let tab_id = snapshot.active_tab_id.as_ref()?;
        let tab = snapshot
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == tab_id)?;
        let layout = tab.terminal_layout()?;
        let pane_id = snapshot
            .focused_pane_id
            .as_ref()
            .filter(|pane_id| layout.contains_pane(pane_id))?;
        let pane = self.workspace_runtime.pane(pane_id)?;
        if pane.tab_id != *tab_id || pane.phase != RuntimePanePhase::Ready {
            return None;
        }
        let session_id = pane.session_id.as_ref()?;
        let registration = self.pane_input_handlers.get(session_id)?;
        let binding = registration.binding.clone();
        if binding.session_id != *session_id
            || binding.owner
                != (SessionOwner::Ui {
                    tab_id: tab_id.clone(),
                    pane_id: pane_id.clone(),
                })
        {
            return None;
        }
        let target = pane.target.as_ref()?;
        let trusted_host = match (target, pane.persisted.target.as_ref()) {
            (SessionTarget::Ssh(ssh), Some(WorkspaceTargetRef::Ssh { host_id }))
                if host_id == &ssh.host_id
                    && pane
                        .persisted
                        .remote_host_id
                        .as_ref()
                        .is_none_or(|persisted_host| persisted_host == host_id) =>
            {
                None
            }
            (SessionTarget::Telnet(_), Some(WorkspaceTargetRef::Telnet { host_id }))
                if pane
                    .persisted
                    .remote_host_id
                    .as_ref()
                    .is_none_or(|persisted_host| persisted_host == host_id) =>
            {
                Some(host_id.clone())
            }
            (SessionTarget::Serial(_), Some(WorkspaceTargetRef::Serial { host_id }))
                if pane
                    .persisted
                    .remote_host_id
                    .as_ref()
                    .is_none_or(|persisted_host| persisted_host == host_id) =>
            {
                Some(host_id.clone())
            }
            (SessionTarget::Local(_), Some(WorkspaceTargetRef::Local { .. }))
            | (SessionTarget::Container(_), Some(WorkspaceTargetRef::DockerContainer { .. })) => {
                None
            }
            _ => return None,
        };
        crate::features::commands::LogsTarget::from_ui_target(binding, target, trusted_host)
    }

    /// Context 导航使用既有业务入口；链接点击另行复核 pane、代次、视口与安全 URL，拒绝过期目标。
    fn handle_context_panel_event(
        &mut self,
        event: crate::features::context::ContextPanelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::features::context::{ContextNavigationTarget, ContextPanelEvent};

        match event {
            ContextPanelEvent::OpenLink {
                identity,
                candidate_id,
            } => self.open_context_terminal_link(&identity, candidate_id, cx),
            ContextPanelEvent::Navigate(target) => match target {
                ContextNavigationTarget::Host(host_id) => {
                    self.selected_host_id = Some(host_id.clone());
                    if let Some(panel) = self.connections_panel.clone() {
                        panel.update(cx, |panel, cx| panel.select_host_id(&host_id, cx));
                        self.set_center_panel(CenterPanel::Connections, window, cx);
                    }
                    self.refresh_context_panel_projection(cx);
                }
                ContextNavigationTarget::Tab(tab_id) => {
                    self.activate_workspace_tab(&tab_id, window, cx);
                }
                ContextNavigationTarget::Pane(pane_id) => {
                    let Some(tab_id) = self
                        .workspace_runtime
                        .pane(&pane_id)
                        .map(|pane| pane.tab_id.clone())
                    else {
                        return;
                    };
                    self.activate_workspace_tab(&tab_id, window, cx);
                    self.focus_workspace_pane(tab_id, pane_id, window, cx);
                }
                ContextNavigationTarget::Agent(agent_session_id) => {
                    if let Some(panel) = self.agents_panel.clone() {
                        panel.update(cx, |panel, cx| {
                            let _ = panel.select_existing_agent(agent_session_id, cx);
                        });
                    } else {
                        self.status = Some(crate::locale::text(
                            "Agent 面板当前不可用，无法打开此会话。",
                            "The Agent panel is unavailable, so this session cannot be opened.",
                        ));
                        cx.notify();
                    }
                }
            },
        }
    }

    /// 只在终端中心仍活动时打开当前 pane 的缓存候选；中心切换后的迟到事件直接失效。
    ///
    /// @author kongweiguang
    fn open_context_terminal_link(
        &self,
        identity: &TerminalLinkSnapshotIdentity,
        candidate_id: TerminalLinkCandidateId,
        cx: &mut Context<Self>,
    ) {
        if self.center_panel != CenterPanel::Terminal {
            return;
        }
        let Some(controller) = self.workspace_controller.as_ref() else {
            return;
        };
        let controller_snapshot = controller.snapshot();
        let Some(tab_id) = controller_snapshot.active_tab_id.as_ref() else {
            return;
        };
        let Some(tab) = controller_snapshot
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == tab_id)
        else {
            return;
        };
        let Some(layout) = tab.terminal_layout() else {
            return;
        };
        let Some(pane_id) = controller_snapshot
            .focused_pane_id
            .as_ref()
            .filter(|focused| layout.contains_pane(focused))
            .cloned()
            .or_else(|| layout.pane_ids().into_iter().next())
        else {
            return;
        };
        let expected_owner = SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        };
        if identity.binding.owner != expected_owner {
            return;
        }

        let Some(runtime_pane) = self.workspace_runtime.pane(&pane_id) else {
            return;
        };
        let Some(runtime_snapshot) = runtime_pane.snapshot.as_ref() else {
            return;
        };
        if runtime_pane.tab_id != *tab_id
            || runtime_pane.phase != RuntimePanePhase::Ready
            || runtime_pane.session_id.as_ref() != Some(&identity.binding.session_id)
            || runtime_snapshot.session_id != identity.binding.session_id
        {
            return;
        }
        let Some(registration) = self.pane_input_handlers.get(&identity.binding.session_id) else {
            return;
        };
        if registration.binding != identity.binding {
            return;
        }
        let current_identity = TerminalLinkSnapshotIdentity {
            binding: registration.binding.clone(),
            revision: runtime_snapshot.revision,
            scrollback_offset: runtime_snapshot.scrollback_offset,
            size: runtime_snapshot.size,
        };
        if &current_identity != identity {
            return;
        }
        let Some(raw_link) = self
            .terminal_link_discovery_caches
            .get(&pane_id)
            .and_then(|cache| cache.resolve(identity, candidate_id))
        else {
            return;
        };
        let Ok(link) = crate::features::terminal_tools::validate_terminal_link(raw_link.as_str())
        else {
            return;
        };
        cx.open_url(link.as_str());
    }

    /// 允许 Context 展示最近一次扫描结果，即使它已 stale；点击时仍必须重新经过完整身份校验。
    ///
    /// @author kongweiguang
    pub(super) fn terminal_link_snapshot_for_context(
        &self,
        pane_id: &PaneId,
    ) -> Option<Arc<TerminalLinkSnapshot>> {
        self.terminal_link_discovery_caches
            .get(pane_id)?
            .snapshot()
            .cloned()
    }
}

impl EventEmitter<RootViewEvent> for RootView {}

impl Drop for RootView {
    /// 根视图销毁时取消所有仍由它持有的订阅令牌，避免后台等待绑定到已关闭窗口。
    fn drop(&mut self) {
        self.agent_watch_cancellation.cancel();
        for cancellation in self.workspace_restore_cancellations.values() {
            cancellation.cancel();
        }
    }
}

/// 优先保留两级真实名称；仅完全相同的标签合并，避免右侧目标行重复显示同一名称。
///
/// @author kongweiguang
fn merge_commands_target_labels(tab_title: &str, pane_title: &str) -> Option<String> {
    let tab_title = tab_title.trim();
    let pane_title = pane_title.trim();
    match (tab_title.is_empty(), pane_title.is_empty()) {
        (true, true) => None,
        (true, false) => Some(pane_title.to_owned()),
        (false, true) => Some(tab_title.to_owned()),
        (false, false) if tab_title == pane_title => Some(tab_title.to_owned()),
        (false, false) => Some(format!("{tab_title} · {pane_title}")),
    }
}

impl Render for RootView {
    /// 实际派发宿主收尾后只绘制停止投影，避免普通子视图每帧重新提交任务；确认和保存准备阶段仍保留原交互。
    /// Workspace 与 Agent 搜索分别挂在自身 Canvas 上，使浮层只覆盖终端内容且不参与布局测量。
    /// 群发栏占真实 flex 槽，候选与现有 pane 都从剩余高度测量 PTY，避免用显示前的旧网格启动。
    /// 容器只由固定右栏挂载；退出准备失败提示保留在工作台，不伪造服务已停止。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(stopping) = self.render_host_shutdown(window, cx) {
            return stopping;
        }
        self.sync_pending_root_confirmation(window, cx);
        self.restore_workspace_files(window, cx);
        self.ensure_workspace_render_entities(window, cx);
        if !self.right_dock_collapsed
            && self.tool_rail_state.attached_tool() == Some(ToolRailToolId::AgentLauncher)
        {
            self.request_agent_snapshot(cx);
        }

        let theme = ThemeTokens::current(window, cx);
        let active_workspace_tab = self.workspace_controller.as_ref().and_then(|controller| {
            let snapshot = controller.snapshot();
            let active_tab_id = snapshot.active_tab_id.as_ref()?;
            snapshot
                .terminal_tabs
                .iter()
                .find(|tab| tab.id() == active_tab_id)
                .cloned()
        });
        if self.center_panel == CenterPanel::Terminal
            && active_workspace_tab.is_none()
            && !window.has_active_prompt()
            && window.focused(cx).is_none()
        {
            // 空工作区才借用 Root 的 Workspace focus handle；已有输入、菜单或终端焦点一律保留。
            window.focus(&self.focus_handle, cx);
        }

        let center_content: AnyElement = match self.center_panel {
            CenterPanel::Terminal => match active_workspace_tab.as_ref() {
                Some(WorkspaceSessionTab::Terminal { id, .. }) => self
                    .workspace_renders
                    .get(id)
                    .cloned()
                    .map(|render| {
                        let broadcast_bar = self.render_workspace_broadcast_bar(id, window, cx);
                        div()
                            .id("active-terminal-workspace")
                            .size_full()
                            .min_w(px(0.))
                            .min_h(px(0.))
                            .flex()
                            .flex_col()
                            .children(broadcast_bar)
                            .child(
                                div()
                                    .id("terminal-pane-region")
                                    .flex_1()
                                    .min_w(px(0.))
                                    .min_h(px(0.))
                                    .overflow_hidden()
                                    .child(render),
                            )
                            .into_any_element()
                    })
                    .unwrap_or_else(|| {
                        empty_workbench_slot(
                            "终端工作区正在准备。",
                            "The terminal workspace is preparing.",
                            theme.muted,
                        )
                    }),
                Some(WorkspaceSessionTab::Unsupported { kind, .. }) => div()
                    .id("unsupported-workspace-tab")
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p_4()
                    .text_color(theme.muted)
                    .child(format!("当前版本不支持此工作区标签：{kind}"))
                    .into_any_element(),
                Some(_) => empty_workbench_slot(
                    "请使用对应的文件工作区查看此标签。",
                    "Open this tab in its matching file workspace.",
                    theme.muted,
                ),
                None => empty_workbench_slot(
                    "从左侧主机导航打开连接，或新建本地终端。",
                    "Open a connection from host navigation or create a local terminal.",
                    theme.muted,
                ),
            },
            CenterPanel::Files => {
                let pane = match &self.files_workspace_selection {
                    workspace_files::FilesWorkspaceSelection::Workbench => self.files_pane.clone(),
                    workspace_files::FilesWorkspaceSelection::TransferTab(tab_id) => {
                        self.workspace_files.files_pane(tab_id)
                    }
                };
                pane.map_or_else(
                    || {
                        empty_workbench_slot(
                            "文件服务尚未配置。",
                            "File services are unavailable.",
                            theme.muted,
                        )
                    },
                    |pane| {
                        div()
                            .id("files-workspace")
                            .size_full()
                            .child(pane)
                            .into_any_element()
                    },
                )
            }
            CenterPanel::Editor => self.file_editor_workspace.clone().map_or_else(
                || {
                    empty_workbench_slot(
                        "文件编辑服务尚未配置。",
                        "The file editor service is unavailable.",
                        theme.muted,
                    )
                },
                |editor| {
                    div()
                        .id("editor-workspace")
                        .size_full()
                        .child(editor)
                        .into_any_element()
                },
            ),
            CenterPanel::Preferences => empty_workbench_slot(
                "设置当前作为模态窗口显示。",
                "Settings are shown in a modal window.",
                theme.muted,
            ),
            CenterPanel::Commands => self.commands_view.clone().map_or_else(
                || {
                    empty_workbench_slot(
                        "命令服务尚未配置。",
                        "Command services are unavailable.",
                        theme.muted,
                    )
                },
                |commands| {
                    div()
                        .id("commands-workspace")
                        .size_full()
                        .child(commands)
                        .into_any_element()
                },
            ),
            CenterPanel::Connections if self.connections_modal_open => empty_workbench_slot(
                "主机编辑器当前作为模态窗口显示。",
                "The host editor is currently shown in a modal window.",
                theme.muted,
            ),
            CenterPanel::Connections => self.connections_panel.clone().map_or_else(
                || {
                    empty_workbench_slot(
                        "主机连接服务尚未配置。",
                        "Host connection services are unavailable.",
                        theme.muted,
                    )
                },
                |connections| {
                    div()
                        .id("connections-workspace")
                        .size_full()
                        .child(connections)
                        .into_any_element()
                },
            ),
            CenterPanel::Agents => empty_workbench_slot(
                "Agent 会话固定显示在右侧工作区。",
                "Agent sessions are shown in the right workspace.",
                theme.muted,
            ),
            CenterPanel::McpRuntime => self.mcp_runtime_panel.clone().map_or_else(
                || {
                    empty_workbench_slot(
                        "MCP runtime control is unavailable.",
                        "MCP runtime control is unavailable.",
                        theme.muted,
                    )
                },
                |panel| {
                    div()
                        .id("mcp-runtime-workspace")
                        .size_full()
                        .child(panel)
                        .into_any_element()
                },
            ),
            CenterPanel::Updates => div()
                .id("updates-workspace")
                .size_full()
                .child(self.updates_panel.clone())
                .into_any_element(),
        };
        // 设置中的辅助页保留明确返回入口，不再依赖已移除的“更多”工作区切换器。
        // 返回只切换视图，原来的终端、分屏和 Agent 目标仍由 Root 持有。
        let center_content = if matches!(
            self.center_panel,
            CenterPanel::McpRuntime | CenterPanel::Updates
        ) {
            div()
                .id("auxiliary-workspace-with-return")
                .size_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .w_full()
                        .flex_shrink_0()
                        .px_2()
                        .py_1()
                        .bg(theme.materials.panel)
                        .child(
                            Button::new("auxiliary-workspace-return-terminal")
                                .compact()
                                .label(crate::locale::text("返回终端", "Back to terminal"))
                                .on_click(cx.listener(|root, _: &ClickEvent, window, cx| {
                                    root.set_center_panel(CenterPanel::Terminal, window, cx);
                                })),
                        ),
                )
                .child(div().flex_1().min_h(px(0.)).child(center_content))
                .into_any_element()
        } else {
            center_content
        };
        let center_key_context = if self.center_panel == CenterPanel::Terminal {
            crate::keybindings::KeybindingContext::Terminal.as_gpui_identifier()
        } else {
            crate::keybindings::KeybindingContext::TextInput.as_gpui_identifier()
        };
        let center_content = div()
            .id("workbench-center-key-context")
            .size_full()
            .key_context(center_key_context)
            .child(center_content)
            .into_any_element();
        let center_content = if self.tool_rail_owns_center_panel() {
            // 片段的同一业务 Entity 只能进入当前配置的一个宿主；SFTP rail 已使用独立浏览实体。
            div()
                .id("tool-rail-owned-center-placeholder")
                .size_full()
                .into_any_element()
        } else {
            center_content
        };

        let agent_terminal_content = self
            .agent_terminal_session_id
            .clone()
            .zip(self.agent_terminal_snapshot.clone())
            .zip(self.agent_terminal_tools.clone())
            .zip(self.selected_agent_session_id.clone())
            .map(|(((session_id, snapshot), tools), agent_session_id)| {
                self.configure_terminal_tools(&tools, Some(session_id.clone()), window, cx);
                let (overlays, search_overlay) = tools.update(cx, |tools, _| {
                    (tools.render_overlays(&snapshot), tools.search_overlay())
                });
                let input_handler = self.ensure_pane_input_handler(
                    session_id.clone(),
                    SessionOwner::Agent { agent_session_id },
                    cx,
                );
                let (focus, ime_preview) = if let Some(input) = input_handler.as_ref() {
                    let binding = input.read(cx).binding().clone();
                    let _ = input.update(cx, |handler, _| {
                        handler.update_projection(
                            &binding,
                            &snapshot.session_id,
                            Some(snapshot.cursor),
                            snapshot.scrollback_offset,
                            self.terminal_metrics.clone(),
                        )
                    });
                    let input = input.read(cx);
                    let preview = input
                        .ime_preview()
                        .map(|preview| (preview.text, preview.selection_bytes));
                    (input.focus_handle(), preview)
                } else {
                    (self.focus_handle.clone(), None)
                };
                let blink_visible = !self.cursor_blink_enabled
                    || !snapshot.cursor.blinking
                    || self.cursor_blink_phase_visible;
                let right_click_behavior = self
                    .settings_snapshot
                    .as_ref()
                    .map(|settings| settings.terminal.right_click_behavior)
                    .unwrap_or_default();
                #[cfg(target_os = "macos")]
                let mac_option_is_meta = preferences::interaction_policy(self)
                    .map(|policy| policy.mac_option_is_meta())
                    .unwrap_or_else(|| TerminalSettings::default().mac_option_is_meta);
                #[cfg(not(target_os = "macos"))]
                let mac_option_is_meta = false;
                let canvas = terminal_canvas(
                    TerminalCanvasFrame {
                        theme: ThemeTokens::current(window, cx),
                        view_binding: self.terminal_view_bindings.get(&session_id).cloned(),
                        snapshot: Some(snapshot.clone()),
                        ime_preview,
                        mac_option_is_meta,
                        metrics: self.terminal_metrics.clone(),
                        overlays,
                        cursor_blink_phase_visible: blink_visible,
                    },
                    cx.entity(),
                    input_handler.clone(),
                    tools.clone(),
                    focus.clone(),
                    right_click_behavior,
                );
                let mut canvas_slot = div()
                    .id("agent-terminal-canvas-slot")
                    .w_full()
                    .flex_1()
                    .relative()
                    .min_w(px(0.))
                    .min_h(px(0.))
                    .overflow_hidden()
                    // 画布退出正常流，由已分配剩余高度的 slot 提供四边约束，避免百分比高度循环解析为零。
                    .child(div().absolute().inset_0().child(canvas));
                if let Some(input) = input_handler {
                    let key_down_input = input.clone();
                    let key_up_input = input.clone();
                    canvas_slot = canvas_slot
                        .track_focus(&focus)
                        .on_key_down(cx.listener(move |root, event, window, cx| {
                            root.handle_pane_key_down(key_down_input.clone(), event, window, cx);
                        }))
                        .on_key_up(cx.listener(move |root, event, window, cx| {
                            root.handle_pane_key_up(key_up_input.clone(), event, window, cx);
                        }));
                }
                if let Some(search) = search_overlay {
                    let placement = search.read(cx).overlay_placement();
                    canvas_slot = canvas_slot.child(terminal_search_overlay_element(
                        search,
                        placement,
                        &self.terminal_metrics,
                    ));
                }
                div()
                    .id("agent-terminal-content")
                    .w_full()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .bg(terminal_background_color(snapshot.palette)
                        .opacity(theme.materials.terminal_body_opacity))
                    .child(canvas_slot)
                    .key_context(
                        crate::keybindings::KeybindingContext::Terminal.as_gpui_identifier(),
                    )
                    .into_any_element()
            });

        let tool_panels = self.workbench_tool_panels(window, cx);
        let workbench = workbench::render_workbench(
            self,
            self.host_navigation.clone(),
            center_content,
            agent_terminal_content,
            tool_panels,
            window,
            cx,
        );
        let workbench = div()
            .id("workbench-app-key-context")
            .size_full()
            .key_context(crate::keybindings::KeybindingContext::App.as_gpui_identifier())
            .child(
                div()
                    .id("workbench-workspace-key-context")
                    .size_full()
                    .key_context(
                        crate::keybindings::KeybindingContext::Workspace.as_gpui_identifier(),
                    )
                    .track_focus(&self.focus_handle)
                    .on_key_down(cx.listener(|root, event, window, cx| {
                        root.handle_workbench_key_down(event, window, cx);
                    }))
                    .child(workbench),
            );

        let mut workbench_shell = div()
            .id("workbench-with-orphan-cleanup")
            .size_full()
            .flex()
            .flex_col()
            .relative();
        if let Some(notice) = self.render_quit_preparation_failure(window, cx) {
            workbench_shell = workbench_shell.child(notice);
        }
        if let Some(panel) = self
            .workspace_orphan_cleanup_panel
            .as_ref()
            .filter(|panel| !panel.read(cx).is_empty())
        {
            workbench_shell = workbench_shell.child(panel.clone());
        }
        if let Some(notice) = self.initial_proxy_notice.as_ref() {
            let message = notice.message.clone();
            let retryable = notice.retry_intent.is_some()
                && !notice.retrying
                && self.initial_proxy_binding_is_current(&notice.binding);
            let mut notice_row = div()
                .id("initial-ssh-proxy-result")
                .w_full()
                .px_3()
                .py_2()
                .flex()
                .items_center()
                .gap_3()
                .bg(theme.materials.panel)
                .border_b_1()
                .border_color(theme.sidebar)
                .child(div().flex_1().text_color(theme.muted).child(message));
            if retryable {
                notice_row = notice_row.child(
                    Button::new("initial-ssh-proxy-retry")
                        .compact()
                        .label(crate::locale::text("重试代理", "Retry proxy"))
                        .on_click(cx.listener(|root, _: &ClickEvent, _, cx| {
                            root.retry_initial_proxy_policy(cx);
                        })),
                );
            }
            workbench_shell = workbench_shell.child(notice_row);
        }
        workbench_shell = workbench_shell.child(
            div()
                .id("workbench-interactive-content")
                .flex_1()
                .min_h(px(0.))
                .child(workbench),
        );
        if !self.preferences_modal_open
            && !self.connections_modal_open
            && let Some(quick_open) = self.quick_open_view.clone()
        {
            workbench_shell = workbench_shell.child(quick_open);
        }
        let background = crate::terminal_background::render_layers(
            preferences::background_settings(self),
            theme,
        );
        let background_status = if self.preferences_modal_open || self.connections_modal_open {
            None
        } else {
            background.status
        };
        div()
            .id("workbench-background-composition")
            .size_full()
            .relative()
            .overflow_hidden()
            // 捕获阶段先于 Editor/File 子控件；仅组件确认其仍拥有 Tab/Group drag 时截断 Escape。
            .capture_key_down(
                cx.listener(|root, event: &gpui_kit::KeyDownEvent, window, cx| {
                    if !event.is_held
                        && event.keystroke.key.eq_ignore_ascii_case("escape")
                        && root.cancel_workspace_tab_drag(window, cx)
                    {
                        root.keybinding_runtime.clear_pending();
                        window.prevent_default();
                        cx.stop_propagation();
                    }
                }),
            )
            .child(
                div()
                    .id("workbench-background-layer")
                    .absolute()
                    .inset_0()
                    .size_full()
                    .child(background.underlay),
            )
            .child(
                div()
                    .id("workbench-interactive-layer")
                    .absolute()
                    .inset_0()
                    .size_full()
                    .child(workbench_shell),
            )
            .when_some(background_status, |root, status| {
                root.child(
                    div()
                        .id("workbench-background-status")
                        .absolute()
                        .inset_0()
                        .size_full()
                        .child(status),
                )
            })
            .into_any_element()
    }
}

/// 空工作区只说明当前没有可用资源，不伪造 renderer 或绑定到旧 SessionId。
fn empty_workbench_slot(
    message: &'static str,
    english_message: &'static str,
    color: gpui_kit::Hsla,
) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p_4()
        .text_color(color)
        .child(crate::locale::text(message, english_message))
        .into_any_element()
}
/// 把 GPUI 修饰键统一映射到终端协议，不让 engine 依赖窗口框架类型。
fn input_modifiers(modifiers: gpui_kit::Modifiers) -> InputModifiers {
    InputModifiers {
        shift: modifiers.shift,
        alt: modifiers.alt,
        control: modifiers.control,
        super_key: modifiers.platform,
    }
}

/// 将 GPUI 键名转换成平台无关键标识；未知按键留给输入法文本通道。
fn terminal_key(key: &str) -> Option<TerminalKey> {
    let normalized = key.to_ascii_lowercase();
    Some(match normalized.as_str() {
        "enter" | "return" => TerminalKey::Enter,
        "tab" => TerminalKey::Tab,
        "backspace" => TerminalKey::Backspace,
        "escape" | "esc" => TerminalKey::Escape,
        "up" | "arrowup" => TerminalKey::Up,
        "down" | "arrowdown" => TerminalKey::Down,
        "left" | "arrowleft" => TerminalKey::Left,
        "right" | "arrowright" => TerminalKey::Right,
        "home" => TerminalKey::Home,
        "end" => TerminalKey::End,
        "insert" => TerminalKey::Insert,
        "delete" => TerminalKey::Delete,
        "pageup" => TerminalKey::PageUp,
        "pagedown" => TerminalKey::PageDown,
        "space" => TerminalKey::Character(' '),
        name if name.starts_with('f') => TerminalKey::Function(name[1..].parse().ok()?),
        _ => {
            let mut characters = normalized.chars();
            let character = characters.next()?;
            if characters.next().is_some() {
                return None;
            }
            TerminalKey::Character(character)
        }
    })
}

/// 仅把 UI 能按事件安全追踪的普通编辑标为 Tracked；粘贴、IME、修饰键和交互控制使正文投影失效。
fn tracked_projection_for_input(
    owner: &SessionOwner,
    event: &PaneTerminalInputEvent,
) -> UiInputProjection {
    if !matches!(owner, SessionOwner::Ui { .. }) {
        return UiInputProjection::Invalidates;
    }
    let tracked = match (&event.event, event.text_origin) {
        (TerminalInputEvent::Text(_), Some(PaneTextOrigin::Plain)) => true,
        (TerminalInputEvent::KeyWithText { event, .. }, None) => {
            event.phase == KeyPhase::Release
                || (!event.modifiers.control && !event.modifiers.alt && !event.modifiers.super_key)
        }
        (TerminalInputEvent::Key(key), None) => {
            key.phase == KeyPhase::Release
                || (key.modifiers == InputModifiers::default()
                    && matches!(
                        key.key,
                        TerminalKey::Backspace
                            | TerminalKey::Delete
                            | TerminalKey::Left
                            | TerminalKey::Right
                            | TerminalKey::Home
                            | TerminalKey::End
                    ))
        }
        _ => false,
    };
    if tracked {
        UiInputProjection::Tracked
    } else {
        UiInputProjection::Invalidates
    }
}

/// 把只读恢复提示占用的垂直空间从 PTY 网格中扣除，避免窗口显示行数与真实尺寸错位。
fn terminal_size_for_window(
    window: &mut Window,
    reserved_height: f32,
    metrics: &TerminalMetrics,
) -> TerminalSize {
    let cell_width = metrics.cell_width(window).as_f32();
    let bounds = window.bounds();
    let content_width =
        (bounds.size.width.as_f32() - SIDEBAR_WIDTH - TOOL_RAIL_WIDTH - metrics.inset * 2.0)
            .max(cell_width);
    let content_height = (bounds.size.height.as_f32()
        - TAB_BAR_HEIGHT
        - STATUS_BAR_HEIGHT
        - metrics.inset * 2.0
        - reserved_height)
        .max(metrics.line_height);
    let columns = (content_width / cell_width)
        .floor()
        .clamp(1.0, f32::from(u16::MAX)) as u16;
    let rows = (content_height / metrics.line_height)
        .floor()
        .clamp(1.0, f32::from(u16::MAX)) as u16;
    TerminalSize::new(
        columns,
        rows,
        content_width.min(f32::from(u16::MAX)) as u16,
        content_height.min(f32::from(u16::MAX)) as u16,
    )
    .unwrap_or_default()
}

/// 只接收普通相对路径组件，阻止恢复提示意外暴露配置根或路径逃逸内容。
///
/// @author kongweiguang
fn safe_relative_configuration_path(value: &str) -> String {
    let components = value.split(['/', '\\']).collect::<Vec<_>>();
    if value.is_empty()
        || value.starts_with('/')
        || value.starts_with('\\')
        || value.contains(':')
        || components.is_empty()
        || components.iter().any(|component| {
            component.is_empty()
                || matches!(*component, "." | "..")
                || component.chars().any(char::is_control)
        })
    {
        return "（无效相对路径）".to_owned();
    }

    components.join("/")
}

/// 恢复模式固定展示所有安全诊断，并据此为终端行数预留同等空间。
fn recovery_banner_height(state: &ConfigurationRecoveryState) -> f32 {
    if state.mode != ConfigurationMode::ReadOnlyRecovery {
        return 0.0;
    }
    RECOVERY_BANNER_HEADER_HEIGHT + RECOVERY_ISSUE_ROW_HEIGHT * state.issues.len() as f32
}

#[cfg(test)]
#[path = "../tests/unit/root_commands_session_binding.rs"]
mod commands_session_binding_tests;

/// 仅对需要用户处理的读取状态占用工作区区域；成功加载是常态，不应长期挤压编辑空间。
/// 无效、未来版本和读取错误仍需保留原数据未修改的提示，不能与成功静默规则混为一谈。
///
/// @author kongweiguang
fn workspace_session_notice(
    state: Option<&kerminal_application::workspace::WorkspaceSessionLoad>,
    error: Option<&str>,
) -> Option<SharedString> {
    use kerminal_application::workspace::{WorkspaceSessionInvalidReason, WorkspaceSessionLoad};

    if let Some(error) = error {
        return Some(format!("工作区读取失败，原数据未修改：{error}").into());
    }
    match state? {
        WorkspaceSessionLoad::Missing => None,
        WorkspaceSessionLoad::Loaded { .. } => None,
        WorkspaceSessionLoad::Invalid { reason } => {
            let reason = match reason {
                WorkspaceSessionInvalidReason::MalformedJson => "JSON 格式错误",
                WorkspaceSessionInvalidReason::InvalidRoot => "根结构无效",
                WorkspaceSessionInvalidReason::InvalidSchema => "必需字段或布局无效",
                WorkspaceSessionInvalidReason::CredentialMaterial => "包含禁止保存的凭据正文",
            };
            Some(format!("工作区文件无效（{reason}）；原数据未修改。").into())
        }
        WorkspaceSessionLoad::Unsupported { version } => {
            Some(format!("当前版本不支持工作区版本 v{version}；原数据未修改。").into())
        }
    }
}
