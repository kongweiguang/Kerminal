//! 偏好设置面板的用例协调、草稿状态与配置恢复边界。
//!
//! @author kongweiguang

use std::collections::BTreeMap;

use gpui_kit::{
    AnyWindowHandle, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable as _,
    SharedString, Subscription, Window,
    component::{
        input::{InputEvent, InputState, TextareaState},
        searchable_list::SearchableListItem,
        select::{SelectEvent, SelectState},
        slider::{SliderEvent, SliderState, SliderValue},
    },
};
use kerminal_application::{
    AppError, ApplicationTaskExecutor, PublicErrorCode,
    configuration_changes::ConfigurationChangeService,
    configuration_directory::ConfigurationDirectoryService,
    configuration_sync::{
        ConfigurationSyncOperationResult, ConfigurationSyncService, ConfigurationSyncStatus,
    },
    connections::credentials::{VaultKeyAvailability, VaultKeyManagementService},
    connections::management::HostManagementService,
    platform::ExternalProtocolService,
    preferences::{
        AgentLauncherSettings, InterfaceLanguage, PreferencesService, ProfileDraft, ProfileService,
        SettingsSnapshot, TerminalColorScheme, VersionedProfile, VersionedSettings,
        WorkbenchLayoutSettings,
    },
};
use kerminal_domain::preferences::{
    ConfigRevision, ConfigurationIssueKind, ConfigurationRecoveryState, TerminalProfile,
};
use tokio_util::sync::CancellationToken;

use super::{ConfigDraft, DraftSnapshot, ProfileIssue, SettingsIssue};

#[path = "panel/advanced.rs"]
mod advanced;
#[path = "panel/autosave.rs"]
mod autosave;
#[path = "panel/changes.rs"]
mod changes;
#[path = "panel/close.rs"]
mod close;
#[path = "panel/color_sampling.rs"]
mod color_sampling;
#[path = "panel/connectivity.rs"]
mod connectivity;
#[path = "panel/custom_agent.rs"]
mod custom_agent;
#[path = "panel/desktop_about.rs"]
mod desktop_about;
#[path = "panel/forms.rs"]
mod forms;
#[path = "panel/mcp.rs"]
mod mcp;
pub(in crate::features::preferences) use mcp::McpConfigTarget;
#[path = "panel/profiles.rs"]
mod profiles;
#[path = "panel/protocol.rs"]
pub(super) mod protocol;
#[path = "panel/recovery.rs"]
mod recovery;
#[path = "panel/settings.rs"]
mod settings;
#[path = "panel/shortcut_capture.rs"]
mod shortcut_capture;
#[path = "panel/sync.rs"]
mod sync;
#[path = "panel/terminal.rs"]
pub(super) mod terminal;
#[path = "panel/vault.rs"]
mod vault;
#[path = "panel/workbench_layout.rs"]
mod workbench_layout;
use forms::{set_input, set_textarea};

/// 偏好页面只向根视图发出已保存主题和返回意图，不直接重配整个工作台。
#[derive(Clone, Debug, PartialEq)]
pub enum PreferencesEvent {
    /// 确认 Profile 配置变化后刷新共享目录，不触碰已有终端会话。
    ProfilesChanged,
    /// 首次读取完成后向根视图提供完整运行设置，覆盖异步构造后的订阅时序。
    SettingsLoaded(SettingsSnapshot),
    /// 有效输入立即投影到工作台，持久化确认独立于运行态预览。
    SettingsPreview(SettingsSnapshot),
    /// 仅在 application 确认 settings TOML 已 CAS 保存后发送完整快照供根视图增量应用。
    SettingsSaved(SettingsSnapshot),
    /// 自定义 Agent 管理器专用确认；仅在 CAS 成功或无变化且设置快照已核实时发送。
    AgentLauncherSaved(AgentLauncherSettings),
    /// 管理器保存失败保留稳定机器码，Root 按冲突、未知结果等不同恢复路径处理。
    AgentLauncherSaveFailed {
        code: PublicErrorCode,
        message: String,
    },
    /// 管理器只读回自身提交涉及的最新 Agent launcher 配置，不替换其它偏好草稿。
    AgentLauncherReadBack {
        settings: AgentLauncherSettings,
        retry_allowed: bool,
    },
    /// 管理器读取当前配置失败；不能据此启用上次未知写入的重试。
    AgentLauncherReadBackFailed { message: String },
    /// Root 快捷调整工具栏的 CAS 写入失败时单独回报，保证面板隐藏时也能展示恢复入口。
    ToolRailSaveFailed { message: String },
    /// 只确认此 request 的持久化布局；Root 不得用旧收据恢复较新的运行态布局。
    WorkbenchLayoutPersisted {
        request_id: u64,
        layout: WorkbenchLayoutSettings,
    },
    /// 布局写入失败仍保留最后意图；未知或冲突必须先核对，不能自动重放。
    WorkbenchLayoutSaveFailed {
        request_id: u64,
        code: PublicErrorCode,
        message: String,
    },
    /// 用户完成 dirty-close 选择后请求 Root 恢复打开设置前的工作区页面。
    CloseRequested,
    /// 确认层显隐变化时更新 Root 焦点陷阱，避免同时注册父子陷阱。
    LeaveConfirmationChanged,
    /// 未保存草稿需要退出确认；Root 确保设置窗口可见并保留当前退出意图。
    QuitConfirmationRequested,
    /// 用户确认丢弃偏好草稿后，根视图继续检查其它编辑器并串行执行退出。
    QuitConfirmed,
    /// 用户取消偏好草稿的退出确认，RootView 保留当前应用状态。
    QuitCancelled,
    /// 有后台事务或未知回执时暂缓退出；根视图清除本次意图但保留更新请求。
    QuitDeferred { message: String },
}

/// 成功反馈保留业务语义，让 footer 在当前语言下渲染而非缓存旧语言字符串。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SettingsSuccessStatus {
    Saved,
    Reloaded,
}

impl SettingsSuccessStatus {
    /// 按当前语言即时生成反馈；保存状态使用 main 的短标签，切换语言后不保留旧字符串。
    ///
    /// @author kongweiguang
    pub(super) fn message(self, language: crate::locale::UiLanguage) -> &'static str {
        match (self, language) {
            (Self::Saved, crate::locale::UiLanguage::ZhCn) => "设置已保存",
            (Self::Saved, crate::locale::UiLanguage::EnUs) => "Settings saved",
            (Self::Reloaded, crate::locale::UiLanguage::ZhCn) => "设置已重新载入。",
            (Self::Reloaded, crate::locale::UiLanguage::EnUs) => "Settings reloaded.",
        }
    }
}

/// 配置根目录请求状态区分尚未返回与仅被操作系统受理，避免声称文件管理器已显示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RecoveryDirectoryStatus {
    Requesting,
    Accepted,
    Failed(String),
}

/// 语言下拉的内部值保持稳定枚举，显示标签从当前全局语言动态生成。
///
/// @author kongweiguang
#[derive(Clone)]
pub(super) struct InterfaceLanguageOption {
    value: InterfaceLanguage,
}

impl SearchableListItem for InterfaceLanguageOption {
    type Value = InterfaceLanguage;

    /// 搜索标签跟随应用当前语言变化，持久化值仍使用稳定枚举而非翻译文案。
    ///
    /// @author kongweiguang
    fn title(&self) -> SharedString {
        match self.value {
            InterfaceLanguage::System => crate::locale::text("跟随系统", "System"),
            InterfaceLanguage::ZhCn => crate::locale::text("简体中文", "Simplified Chinese"),
            InterfaceLanguage::EnUs => crate::locale::text("英语", "English"),
        }
    }

    /// 下拉确认只回传配置枚举，避免将本地化标签写回 settings 文件。
    ///
    /// @author kongweiguang
    fn value(&self) -> &Self::Value {
        &self.value
    }
}

/// 固定顺序构造三种界面语言，保证下拉显示、搜索和保存使用同一枚举集合。
///
/// @author kongweiguang
fn interface_language_options() -> Vec<InterfaceLanguageOption> {
    [
        InterfaceLanguage::System,
        InterfaceLanguage::ZhCn,
        InterfaceLanguage::EnUs,
    ]
    .into_iter()
    .map(|value| InterfaceLanguageOption { value })
    .collect()
}

/// 当前 OS 选择对应平台的快捷键冲突校验规则，编辑页仍可配置全部平台。
///
/// @author kongweiguang
pub(super) fn keyboard_platform() -> super::KeyboardPlatform {
    #[cfg(target_os = "macos")]
    {
        super::KeyboardPlatform::Mac
    }
    #[cfg(target_os = "windows")]
    {
        super::KeyboardPlatform::Windows
    }
    #[cfg(target_os = "linux")]
    {
        super::KeyboardPlatform::Linux
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        super::KeyboardPlatform::Windows
    }
}

/// 将稳定错误类别映射为双语安全提示；中文保留 application 脱敏上下文，英语不泄露原文。
pub(super) fn localized_error(error: &AppError) -> String {
    let english = match error.public_code() {
        PublicErrorCode::Invalid => "The input is invalid. Review the field guidance.",
        PublicErrorCode::NotFound => "The item no longer exists. Reload and try again.",
        PublicErrorCode::Unsupported | PublicErrorCode::CapabilityNotSupported => {
            "This operation is not supported in the current context."
        }
        PublicErrorCode::Conflict => {
            "The configuration changed externally. Reload it before continuing."
        }
        PublicErrorCode::Cancelled => "The operation was cancelled.",
        PublicErrorCode::Infrastructure => {
            "A storage or system operation failed. Check availability and try again."
        }
        PublicErrorCode::OutcomeUnknown => {
            "The result is unknown. Read the affected state back before retrying."
        }
        PublicErrorCode::AuthenticationFailed => "Authentication failed.",
        PublicErrorCode::HostKeyUnknown => "The host key has not been trusted.",
        PublicErrorCode::HostKeyChanged => "The host key changed. The connection was blocked.",
        PublicErrorCode::HostKeyTrustMismatch => {
            "Host key verification did not match. Trust was not saved."
        }
        PublicErrorCode::Timeout => "The operation timed out.",
        PublicErrorCode::RemoteExit => "The remote command exited.",
        PublicErrorCode::ConfigurationCorrupt => {
            "The configuration is damaged and requires recovery."
        }
    };
    crate::locale::text(error.public_message(), english).to_string()
}

/// 偏好页面的导航区域，各动作分别调用配置或凭据应用用例。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PreferencesSection {
    Appearance,
    Terminal,
    KeywordHighlights,
    CommandSuggestions,
    Sftp,
    ExternalLaunch,
    Mcp,
    Desktop,
    About,
    Shortcuts,
    Profiles,
    Integrations,
    ConfigurationSync,
}

/// settings 文本输入的领域字段映射；解析失败时保留 UI 草稿文本并阻止保存。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SettingsTextField {
    FontFamily,
    FontSize,
    LineHeight,
    Scrollback,
    BackgroundPath,
    BackgroundOpacity,
    WindowOpacity,
    SftpGlobalTransfers,
    SftpHostTransfers,
    SftpPipelineDepth,
    SftpPacketBytes,
    SftpIdleTimeout,
    NotificationMinDuration,
    NotificationThrottle,
}

/// Profile 表单输入字段，args/env 按行编辑并在进入 application 用例前解析。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProfileTextField {
    Name,
    Shell,
    WorkingDirectory,
    Arguments,
    Environment,
}

/// 自定义启动器的草稿字段在入库前单独校验，不混入已持久化的 Agent 定义。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AgentLauncherTextField {
    Name,
    Command,
}

/// 平台快捷键字段区分 Windows、Linux 与 macOS，保留 Option 的缺省/显式空值语义。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ShortcutBindingField {
    Windows,
    Linux,
    Mac,
}

/// 平台绑定写入意图；只有 Linux 允许清除覆盖并恢复 Windows 字段回退。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::features::preferences) enum ShortcutBindingUpdate {
    /// 为该平台写入明确的按键组合。
    Set(String),
    /// 仅 Linux：删除专用覆盖，重新跟随回退语义。
    Inherit,
}

impl ShortcutBindingField {
    /// 由设置页平台还原持久字段，避免调用方按字符串或位置猜测。
    ///
    /// @author kongweiguang
    pub(super) const fn from_editor_platform(platform: super::KeyboardPlatform) -> Self {
        match platform {
            super::KeyboardPlatform::Windows => Self::Windows,
            super::KeyboardPlatform::Linux => Self::Linux,
            super::KeyboardPlatform::Mac => Self::Mac,
        }
    }

    /// 设置页选项与持久字段共用同一枚举身份，避免渲染和写入各维护一份映射。
    ///
    /// @author kongweiguang
    pub(super) const fn editor_platform(self) -> super::KeyboardPlatform {
        match self {
            Self::Windows => super::KeyboardPlatform::Windows,
            Self::Linux => super::KeyboardPlatform::Linux,
            Self::Mac => super::KeyboardPlatform::Mac,
        }
    }

    /// 录制、恢复默认和冲突校验必须命中同一个平台快照，避免跨平台误判。
    ///
    /// @author kongweiguang
    pub(super) const fn platform(self) -> crate::keybindings::KeybindingPlatform {
        match self {
            Self::Windows => crate::keybindings::KeybindingPlatform::Windows,
            Self::Linux => crate::keybindings::KeybindingPlatform::Linux,
            Self::Mac => crate::keybindings::KeybindingPlatform::Mac,
        }
    }

    /// 错误键使用稳定的配置字段名，不跟随界面语言或平台名称变化。
    ///
    /// @author kongweiguang
    pub(super) const fn key(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Linux => "linux",
            Self::Mac => "macos",
        }
    }
}

/// 固定设置字段使用 GPUI Kit 输入状态，便于在异步 reload/save 后同步控件值。
pub(super) struct SettingsInputs {
    pub(super) font_family: Entity<InputState>,
    pub(super) font_size: Entity<InputState>,
    pub(super) line_height: Entity<InputState>,
    pub(super) scrollback: Entity<InputState>,
    pub(super) background_path: Entity<InputState>,
    pub(super) background_opacity: Entity<InputState>,
    pub(super) window_opacity: Entity<InputState>,
    pub(super) sftp_global_transfers: Entity<InputState>,
    pub(super) sftp_host_transfers: Entity<InputState>,
    pub(super) sftp_pipeline_depth: Entity<InputState>,
    pub(super) sftp_packet_bytes: Entity<InputState>,
    pub(super) sftp_idle_timeout: Entity<InputState>,
    pub(super) notification_min_duration: Entity<InputState>,
    pub(super) notification_throttle: Entity<InputState>,
}

/// Profile 表单输入状态。环境变量只编辑明文配置值，不包含或接触 vault 凭据。
pub(super) struct ProfileInputs {
    pub(super) name: Entity<InputState>,
    pub(super) shell: Entity<InputState>,
    pub(super) working_directory: Entity<InputState>,
    pub(super) arguments: Entity<TextareaState>,
    pub(super) environment: Entity<TextareaState>,
}

/// Agent Launcher 两个临时输入不进入设置草稿，只有点击添加后才成为可保存的定义。
pub(super) struct AgentLauncherInputs {
    pub(super) name: Entity<InputState>,
    pub(super) command: Entity<InputState>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LeaveConfirmation {
    Close,
    Quit,
}

/// 每种确认各自拥有焦点身份和恢复目标，避免迟到回调覆盖后来的弹层。
pub(super) struct LeaveConfirmationFocus {
    pub(super) container: FocusHandle,
    pub(super) keep_editing: FocusHandle,
    pub(super) discard: FocusHandle,
    return_focus: Option<FocusHandle>,
    generation: u64,
}

impl LeaveConfirmationFocus {
    /// 焦点句柄在面板生命周期内稳定，按钮可在弹层首次绘制后立即接收焦点。
    ///
    /// @author kongweiguang
    fn new(cx: &mut Context<PreferencesPanel>) -> Self {
        Self {
            container: cx.focus_handle(),
            keep_editing: cx.focus_handle().tab_stop(true),
            discard: cx.focus_handle().tab_stop(true),
            return_focus: None,
            generation: 0,
        }
    }
}

/// settings 与 Profile 页面状态只保存 typed 快照和 opaque revision，文件访问由 application 负责。
pub struct PreferencesPanel {
    screen_color_sampling: color_sampling::ScreenColorSamplingState,
    settings_autosave: autosave::SettingsAutosaveState,
    pub(super) keyword_editor: terminal::KeywordEditorState,
    pub(super) suggestion_selects: terminal::SuggestionSelects,
    pub(super) connectivity: connectivity::ConnectivityState,
    pub(super) mcp_settings: mcp::McpSettingsState,
    pub(super) desktop_about: desktop_about::DesktopAboutState,
    pub(super) shortcut_platform: super::KeyboardPlatform,
    pub(super) shortcut_capture: Option<shortcut_capture::ShortcutCapture>,
    pub(super) shortcut_capture_subscription: Option<Subscription>,
    pub(super) shortcut_field_errors: BTreeMap<String, String>,
    pub(super) inline_updates: Option<Entity<crate::features::updates::UpdatesPanel>>,
    pub(super) _inline_updates_subscription: Option<Subscription>,
    preferences: PreferencesService,
    profiles: ProfileService,
    pub(super) host_management: Option<HostManagementService>,
    pub(super) vault_keys: Option<VaultKeyManagementService>,
    pub(super) configuration_sync: Option<ConfigurationSyncService>,
    pub(super) configuration_changes: Option<ConfigurationChangeService>,
    pub(super) external_protocol: Option<ExternalProtocolService>,
    pub(super) external_protocol_status:
        Option<kerminal_application::platform::ExternalProtocolStatus>,
    pub(super) external_protocol_loading: bool,
    pub(super) external_protocol_error: Option<String>,
    pub(super) external_protocol_error_is_operation: bool,
    external_protocol_operation: Option<protocol::ExternalProtocolAction>,
    pub(super) external_protocol_requires_refresh: bool,
    pub(super) external_protocol_outcome_unknown: bool,
    external_protocol_last_result: Option<protocol::ExternalProtocolAction>,
    external_protocol_generation: u64,
    pub(super) mcp_runtime_available: bool,
    pub(super) configuration_directory: Option<ConfigurationDirectoryService>,
    pub(super) recovery_directory_status: Option<RecoveryDirectoryStatus>,
    configuration_change_cancellation: CancellationToken,
    executor: ApplicationTaskExecutor,
    pub(super) recovery: ConfigurationRecoveryState,
    pub(super) theme_mode: super::ThemeMode,
    pub(super) settings: Option<ConfigDraft<SettingsSnapshot, Option<ConfigRevision>>>,
    pub(super) profile_rows: Vec<VersionedProfile>,
    pub(super) profile_draft: Option<ConfigDraft<ProfileDraft, Option<ConfigRevision>>>,
    pub(super) selected_profile_id: Option<String>,
    pub(super) selected_profile_is_default: bool,
    pub(super) profile_operation_running: bool,
    pub(super) profile_advanced_open: bool,
    pub(super) profile_default_outcome_unknown: bool,
    pub(super) profile_delete_outcome_unknown: bool,
    pub(super) profile_unknown_readback: Option<Option<VersionedProfile>>,
    pub(super) section: PreferencesSection,
    pub(super) navigation_search: Entity<InputState>,
    pub(super) navigation_search_query: String,
    pub(super) selected_navigation_target: Option<String>,
    pub(super) pending_navigation_search_scroll: Option<usize>,
    pub(super) _navigation_search_subscription: Subscription,
    pub(super) interface_language_select: Entity<SelectState<Vec<InterfaceLanguageOption>>>,
    pub(super) background_opacity_slider: Entity<SliderState>,
    pub(super) window_opacity_slider: Entity<SliderState>,
    pub(super) _appearance_control_subscriptions: Vec<Subscription>,
    pub(super) content_scroll: gpui_kit::ScrollHandle,
    settings_window: AnyWindowHandle,
    pub(super) settings_panel_focus: FocusHandle,
    pub(super) settings_content_focus: gpui_kit::FocusHandle,
    pub(super) settings_close_focus: gpui_kit::FocusHandle,
    pub(super) close_confirmation_focus: LeaveConfirmationFocus,
    pub(super) quit_confirmation_focus: LeaveConfirmationFocus,
    pub(super) sftp_advanced_open: bool,
    pub(super) vault_availability: Option<VaultKeyAvailability>,
    pub(super) vault_loading: bool,
    pub(super) vault_busy: bool,
    pub(super) vault_outcome_unknown: bool,
    pub(super) vault_status: Option<String>,
    pub(super) vault_error: Option<String>,
    pub(super) sync_status: Option<ConfigurationSyncStatus>,
    pub(super) sync_loading: bool,
    pub(super) sync_result: Option<ConfigurationSyncOperationResult>,
    pub(super) sync_error: Option<String>,
    pub(super) sync_outcome_unknown: bool,
    pub(super) settings_inputs: SettingsInputs,
    pub(super) profile_inputs: ProfileInputs,
    pub(super) agent_launcher_inputs: AgentLauncherInputs,
    pub(super) settings_input_errors: BTreeMap<&'static str, String>,
    pub(super) profile_input_errors: BTreeMap<&'static str, String>,
    pub(super) settings_issues: Vec<SettingsIssue>,
    pub(super) profile_issues: Vec<ProfileIssue>,
    pub(super) status: Option<String>,
    pub(super) settings_success_status: Option<SettingsSuccessStatus>,
    /// 区分工作台右键触发的即时保存，让 Root 获得其失败回执而不改变普通设置保存语义。
    pub(super) pending_tool_rail_save: bool,
    /// 管理器 CAS 事务的自有回执状态；失败读回只投影 Agent 字段，不重置共享设置草稿。
    agent_launcher_manager: custom_agent::AgentLauncherManagerState,
    /// 有界的末意图/在途关联，不复制 ConfigDraft 的保存状态机。
    workbench_layout_requests: workbench_layout::WorkbenchLayoutRequests,
    pub(super) loading_settings: bool,
    pub(super) loading_profiles: bool,
    settings_load_generation: u64,
    profiles_load_generation: u64,
    profiles_watch_generation: u64,
    profiles_watch_in_flight: bool,
    profiles_watch_pending: bool,
    pub(super) settings_load_started: bool,
    pub(super) profiles_load_started: bool,
    pub(super) close_confirmation: bool,
    pub(super) quit_confirmation: bool,
    pub(super) profile_switch_confirmation: bool,
    pub(super) profile_reload_confirmation: bool,
    pub(super) profile_delete_confirmation: bool,
    pub(super) pending_profile_selection: Option<Option<String>>,
    pub(super) profile_list_error: Option<String>,
    pub(super) settings_error: Option<String>,
    // 订阅对象只用于保持 GPUI 输入监听存活；订阅本身不需要被主动读取。
    pub(super) _settings_subscriptions: Vec<Subscription>,
    pub(super) _profile_subscriptions: Vec<Subscription>,
    pub(super) _agent_launcher_subscriptions: Vec<Subscription>,
    pub(super) custom_agent_name: String,
    pub(super) custom_agent_command: String,
}

/// 将偏好面板长期依赖集中装配，避免构造参数次序掩盖各服务的可选性。
///
/// @author kongweiguang
pub(crate) struct PreferencesPanelServices {
    pub(crate) preferences: PreferencesService,
    pub(crate) profiles: ProfileService,
    pub(crate) vault_keys: Option<VaultKeyManagementService>,
    pub(crate) configuration_sync: Option<ConfigurationSyncService>,
    pub(crate) configuration_changes: Option<ConfigurationChangeService>,
    pub(crate) external_protocol: Option<ExternalProtocolService>,
    pub(crate) configuration_directory: Option<ConfigurationDirectoryService>,
    pub(crate) mcp_runtime_available: bool,
    pub(crate) mcp_runtime:
        Option<std::sync::Arc<dyn kerminal_application::mcp_runtime::McpRuntimeControl>>,
    pub(crate) executor: ApplicationTaskExecutor,
}

impl PreferencesPanel {
    /// 使用 composition root 注入的服务；输入和确认焦点由面板保留，避免重绘丢失身份。
    ///
    /// @author kongweiguang
    pub(crate) fn new(
        services: PreferencesPanelServices,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let PreferencesPanelServices {
            preferences,
            profiles,
            vault_keys,
            configuration_sync,
            configuration_changes,
            external_protocol,
            configuration_directory,
            mcp_runtime_available,
            mcp_runtime,
            executor,
        } = services;
        let keyword_editor = terminal::KeywordEditorState::new(window, cx);
        let suggestion_selects = terminal::SuggestionSelects::new(window, cx);
        let mcp_settings = mcp::McpSettingsState::new(window, cx);
        let connectivity =
            connectivity::ConnectivityState::new(mcp_runtime, executor.clone(), window, cx);
        let (settings_inputs, settings_subscriptions) = Self::create_settings_inputs(window, cx);
        Self::configure_settings_number_inputs(&settings_inputs, window, cx);
        let (profile_inputs, profile_subscriptions) = Self::create_profile_inputs(window, cx);
        let (agent_launcher_inputs, agent_launcher_subscriptions) =
            Self::create_agent_launcher_inputs(window, cx);
        let interface_language_select =
            cx.new(|cx| SelectState::new(interface_language_options(), None, window, cx));
        let background_opacity_slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(100.)
                .step(1.)
                .default_value(100.)
        });
        let window_opacity_slider = cx.new(|_| {
            SliderState::new()
                .min(35.)
                .max(100.)
                .step(1.)
                .default_value(100.)
        });
        let appearance_control_subscriptions = vec![
            cx.subscribe(
                &interface_language_select,
                |panel, _, event: &SelectEvent<Vec<InterfaceLanguageOption>>, cx| {
                    if let SelectEvent::Confirm(Some(language)) = event {
                        panel.edit_settings(|settings| settings.interface_language = *language, cx);
                    }
                },
            ),
            cx.subscribe(
                &background_opacity_slider,
                |panel, _, event: &SliderEvent, cx| {
                    if let SliderEvent::Change(SliderValue::Single(value)) = event {
                        let value = value.round().clamp(0., 100.) as u8;
                        panel.edit_settings(|settings| settings.background_opacity = value, cx);
                    }
                },
            ),
            cx.subscribe(
                &window_opacity_slider,
                |panel, _, event: &SliderEvent, cx| {
                    if let SliderEvent::Change(SliderValue::Single(value)) = event {
                        let value = value.round().clamp(35., 100.) as u8;
                        panel.edit_settings(|settings| settings.window_opacity = value, cx);
                    }
                },
            ),
        ];
        let navigation_search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::locale::text("搜索设置", "Search settings"))
        });
        let navigation_search_subscription = cx.subscribe(
            &navigation_search,
            |panel, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let query = input.read(cx).value().to_string();
                    if panel.navigation_search_query != query {
                        panel.update_navigation_search_query(query, cx);
                    }
                }
            },
        );
        let mut panel = Self {
            screen_color_sampling: color_sampling::ScreenColorSamplingState::default(),
            settings_autosave: autosave::SettingsAutosaveState::default(),
            keyword_editor,
            suggestion_selects,
            connectivity,
            desktop_about: desktop_about::DesktopAboutState::default(),
            shortcut_platform: keyboard_platform(),
            mcp_settings,
            shortcut_capture: None,
            shortcut_capture_subscription: None,
            shortcut_field_errors: BTreeMap::new(),
            inline_updates: None,
            _inline_updates_subscription: None,
            recovery: preferences.recovery_state(),
            preferences,
            profiles,
            host_management: None,
            vault_keys,
            configuration_sync,
            configuration_changes,
            external_protocol,
            external_protocol_status: None,
            external_protocol_loading: false,
            external_protocol_error: None,
            external_protocol_error_is_operation: false,
            external_protocol_operation: None,
            external_protocol_requires_refresh: false,
            external_protocol_outcome_unknown: false,
            external_protocol_last_result: None,
            external_protocol_generation: 0,
            mcp_runtime_available,
            configuration_directory,
            recovery_directory_status: None,
            configuration_change_cancellation: CancellationToken::new(),
            executor,
            settings: None,
            theme_mode: super::ThemeMode::System,
            profile_rows: Vec::new(),
            profile_draft: None,
            selected_profile_id: None,
            selected_profile_is_default: false,
            profile_operation_running: false,
            profile_advanced_open: false,
            profile_default_outcome_unknown: false,
            profile_delete_outcome_unknown: false,
            profile_unknown_readback: None,
            section: PreferencesSection::Appearance,
            navigation_search,
            navigation_search_query: String::new(),
            selected_navigation_target: None,
            pending_navigation_search_scroll: None,
            _navigation_search_subscription: navigation_search_subscription,
            interface_language_select,
            background_opacity_slider,
            window_opacity_slider,
            _appearance_control_subscriptions: appearance_control_subscriptions,
            content_scroll: gpui_kit::ScrollHandle::new(),
            settings_window: window.window_handle(),
            settings_panel_focus: cx.focus_handle(),
            settings_content_focus: cx.focus_handle(),
            settings_close_focus: cx.focus_handle(),
            close_confirmation_focus: LeaveConfirmationFocus::new(cx),
            quit_confirmation_focus: LeaveConfirmationFocus::new(cx),
            sftp_advanced_open: false,
            vault_availability: None,
            vault_loading: false,
            vault_busy: false,
            vault_outcome_unknown: false,
            vault_status: None,
            vault_error: None,
            sync_status: None,
            sync_loading: false,
            sync_result: None,
            sync_error: None,
            sync_outcome_unknown: false,
            settings_inputs,
            profile_inputs,
            agent_launcher_inputs,
            settings_input_errors: BTreeMap::new(),
            profile_input_errors: BTreeMap::new(),
            settings_issues: Vec::new(),
            profile_issues: Vec::new(),
            status: None,
            settings_success_status: None,
            pending_tool_rail_save: false,
            agent_launcher_manager: custom_agent::AgentLauncherManagerState::default(),
            workbench_layout_requests: workbench_layout::WorkbenchLayoutRequests::default(),
            loading_settings: false,
            loading_profiles: false,
            settings_load_generation: 0,
            profiles_load_generation: 0,
            profiles_watch_generation: 0,
            profiles_watch_in_flight: false,
            profiles_watch_pending: false,
            settings_load_started: false,
            profiles_load_started: false,
            close_confirmation: false,
            quit_confirmation: false,
            profile_switch_confirmation: false,
            profile_reload_confirmation: false,
            profile_delete_confirmation: false,
            pending_profile_selection: None,
            profile_list_error: None,
            settings_error: None,
            _settings_subscriptions: settings_subscriptions,
            _profile_subscriptions: profile_subscriptions,
            _agent_launcher_subscriptions: agent_launcher_subscriptions,
            custom_agent_name: String::new(),
            custom_agent_command: String::new(),
        };
        panel.ensure_loaded(window, cx);
        panel.load_integrations(cx);
        panel.watch_configuration_changes(cx);
        panel
    }

    /// 保留组合根注入接口供本地主机入口使用；Profile 模板自身不再拥有分组。
    /// @author kongweiguang
    pub fn with_host_management_service(mut self, service: HostManagementService) -> Self {
        self.host_management = Some(service);
        self
    }

    /// 旧入口兼容打开未分组模板，组身份只属于主机，不能写回 Profile 草稿。
    /// @author kongweiguang
    pub fn begin_local_profile_editor(
        &mut self,
        _group_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_section(PreferencesSection::Profiles, cx);
        self.request_profile_selection(None, window, cx);
        self.selected_navigation_target = None;
        cx.notify();
    }

    /// 按需同时读取 settings 与 profile typed 快照；页面不在 GPUI 线程访问文件或仓储。
    pub(super) fn ensure_loaded(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_load_started {
            self.settings_load_started = true;
            self.spawn_settings_load(cx);
        }
        if !self.profiles_load_started {
            self.profiles_load_started = true;
            self.spawn_profile_load(cx);
        }
    }

    /// 根视图在启动设置读取和主题保存事件后同步此值，使面板自身也服从显式浅深主题。
    pub fn set_theme_mode(&mut self, mode: super::ThemeMode, cx: &mut Context<Self>) {
        if self.theme_mode != mode {
            self.theme_mode = mode;
            cx.notify();
        }
    }

    /// 输入只更新结果列表；与 main 一样，用户选择结果后才切页和定位。
    /// @author kongweiguang
    pub(super) fn update_navigation_search_query(&mut self, query: String, cx: &mut Context<Self>) {
        if self.navigation_search_query != query {
            self.navigation_search_query = query;
            cx.notify();
        }
    }

    /// 搜索投影不改变正在浏览的页面和草稿，避免每次键入时正文跳动或失焦。
    /// @author kongweiguang
    pub(super) fn reconcile_navigation_search_selection(
        &mut self,
    ) -> super::render::NavigationSearchSelection {
        super::render::resolve_navigation_search_selection(
            &self.navigation_search_query,
            self.section,
            self.selected_navigation_target.as_deref(),
            self.settings.as_ref().map(|draft| draft.value()),
            self.mcp_runtime_available,
        )
    }

    /// Root 的 settings.keybindings 动作复用面板导航；只切换显示，不保存或丢弃任一草稿，关闭仍走双草稿保护。
    pub fn select_keybindings(&mut self, cx: &mut Context<Self>) {
        self.select_section(PreferencesSection::Shortcuts, cx);
    }

    /// 离开确认优先于普通面板获得焦点，迟到的入口不能覆盖确认内已有的用户选择。
    ///
    /// @author kongweiguang
    pub(crate) fn focus_initial_control(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let confirmation = if self.quit_confirmation {
            Some(&self.quit_confirmation_focus)
        } else if self.close_confirmation {
            Some(&self.close_confirmation_focus)
        } else {
            None
        };
        if let Some(focus) = confirmation {
            if !focus.container.contains_focused(window, cx) {
                window.focus(&focus.keep_editing, cx);
            }
        } else {
            window.focus(&self.settings_close_focus, cx);
        }
    }

    /// Root 据此只保留最内层陷阱，避免 Kit 无序选择嵌套焦点范围。
    ///
    /// @author kongweiguang
    pub(crate) fn has_leave_confirmation(&self) -> bool {
        self.close_confirmation || self.quit_confirmation
    }

    /// 创建确认后捕获可恢复的面板内焦点，并仅在同一代确认仍位于顶层时转移焦点。
    ///
    /// @author kongweiguang
    pub(super) fn schedule_leave_confirmation_focus(
        &mut self,
        confirmation: LeaveConfirmation,
        cx: &mut Context<Self>,
    ) {
        let focus = match confirmation {
            LeaveConfirmation::Close => &mut self.close_confirmation_focus,
            LeaveConfirmation::Quit => &mut self.quit_confirmation_focus,
        };
        focus.generation = focus.generation.wrapping_add(1);
        let generation = focus.generation;
        cx.emit(PreferencesEvent::LeaveConfirmationChanged);
        let owner = cx.weak_entity();
        let window_handle = self.settings_window;
        cx.defer(move |app| {
            let _ = window_handle.update(app, |_, window, app| {
                let _ = owner.update(app, |panel, cx| {
                    if !panel.is_current_leave_confirmation(confirmation, generation) {
                        return;
                    }
                    let return_focus = window
                        .focused(cx)
                        .filter(|focus| panel.settings_panel_focus.contains(focus, window));
                    let focus = match confirmation {
                        LeaveConfirmation::Close => &mut panel.close_confirmation_focus,
                        LeaveConfirmation::Quit => &mut panel.quit_confirmation_focus,
                    };
                    if !focus.container.contains_focused(window, cx) {
                        focus.return_focus = return_focus;
                    }
                    cx.on_next_frame(window, move |panel, window, cx| {
                        if panel.is_current_leave_confirmation(confirmation, generation) {
                            panel.focus_initial_control(window, cx);
                        }
                    });
                });
            });
        });
    }

    /// 仅处理同一代且仍在最上层的确认，旧帧回调不得抢走新弹层焦点。
    ///
    /// @author kongweiguang
    fn is_current_leave_confirmation(
        &self,
        confirmation: LeaveConfirmation,
        generation: u64,
    ) -> bool {
        match confirmation {
            LeaveConfirmation::Close => {
                self.close_confirmation
                    && !self.quit_confirmation
                    && self.close_confirmation_focus.generation == generation
            }
            LeaveConfirmation::Quit => {
                self.quit_confirmation && self.quit_confirmation_focus.generation == generation
            }
        }
    }

    /// 只有原确认仍持有焦点时才恢复，避免被其它 Root 模态阻挡的取消抢焦点。
    ///
    /// @author kongweiguang
    pub(super) fn restore_leave_confirmation_focus(
        &mut self,
        confirmation: LeaveConfirmation,
        cx: &mut Context<Self>,
    ) {
        let focus = match confirmation {
            LeaveConfirmation::Close => &mut self.close_confirmation_focus,
            LeaveConfirmation::Quit => &mut self.quit_confirmation_focus,
        };
        focus.generation = focus.generation.wrapping_add(1);
        let container = focus.container.clone();
        let return_focus = focus.return_focus.take();
        cx.emit(PreferencesEvent::LeaveConfirmationChanged);
        let window_handle = self.settings_window;
        let owner = cx.weak_entity();
        cx.defer(move |app| {
            let _ = window_handle.update(app, |_, window, app| {
                let _ = owner.update(app, |panel, cx| {
                    if !container.contains_focused(window, cx) {
                        return;
                    }
                    if panel.has_leave_confirmation() {
                        panel.focus_initial_control(window, cx);
                    } else if let Some(focus) = return_focus
                        .filter(|focus| panel.settings_panel_focus.contains(focus, window))
                    {
                        window.focus(&focus, cx);
                    } else {
                        window.focus(&panel.settings_close_focus, cx);
                    }
                });
            });
        });
    }

    /// 搜索快捷键不能穿透离开确认；仅无确认时将焦点交给搜索框。
    ///
    /// @author kongweiguang
    pub fn focus_navigation_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_leave_confirmation() {
            let container = if self.quit_confirmation {
                &self.quit_confirmation_focus.container
            } else {
                &self.close_confirmation_focus.container
            };
            if !container.contains_focused(window, cx) {
                self.focus_initial_control(window, cx);
            }
            return;
        }
        let focus = self.navigation_search.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    }

    /// 普通入口像 main 重新挂载设置正文；保留在途保存，并重新核实上次未确认的写入结果。
    ///
    /// @author kongweiguang
    pub(crate) fn begin_modal_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_shortcut_capture(cx);
        self.section = PreferencesSection::Appearance;
        self.navigation_search_query.clear();
        self.selected_navigation_target = None;
        self.pending_navigation_search_scroll = None;
        self.profile_advanced_open = false;
        set_input(&self.navigation_search, String::new(), window, cx);
        self.reset_page_disclosures();
        self.content_scroll.scroll_to_top_of_item(0);
        self.recover_pending_settings_on_open(cx);
        self.refresh_integrations_on_modal_open(cx);
        cx.notify();
    }

    /// main 的条件页面在切页时卸载 disclosure；运行态服务与保存任务不属于这些呈现状态。
    ///
    /// @author kongweiguang
    pub(super) fn reset_page_disclosures(&mut self) {
        self.screen_color_sampling.cancel();
        self.sftp_advanced_open = false;
        self.desktop_about.advanced_expanded = false;
        self.connectivity.external_compatibility_expanded = false;
        self.connectivity.mcp_connection_expanded = false;
    }

    /// 原地刷新所有缓存输入的本地化占位符，不重建实体、查询、草稿或焦点。
    ///
    /// @author kongweiguang
    pub fn refresh_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.navigation_search.update(cx, |input, cx| {
            input.set_placeholder(
                crate::locale::text("搜索设置", "Search settings"),
                window,
                cx,
            );
        });
        self.refresh_locale_placeholders(window, cx);
        cx.notify();
    }

    /// 子控件先消费浮层 Escape；到达面板后撤销确认并恢复焦点，避免越过草稿保护。
    ///
    /// @author kongweiguang
    pub fn handle_escape(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.screen_color_sampling.cancel() {
            cx.notify();
            return;
        }
        // 关闭最上层确认框优先于搜索清理或页面关闭，避免 Escape 越过用户确认。
        if self.quit_confirmation {
            self.cancel_quit(cx);
            return;
        }
        if self.profile_delete_confirmation {
            self.cancel_delete_profile(cx);
            return;
        }
        if self.profile_reload_confirmation {
            self.cancel_profile_reload(cx);
            return;
        }
        if self.profile_switch_confirmation {
            self.profile_switch_confirmation = false;
            self.pending_profile_selection = None;
            cx.notify();
            return;
        }
        if self.close_confirmation {
            self.cancel_close(cx);
            return;
        }
        self.request_close(cx);
    }

    /// 侧栏与根快捷键共享导航变更；切换仅改视图选择并清除字段级高亮，不影响草稿或 revision。
    ///
    /// @author kongweiguang
    pub(super) fn select_section(&mut self, section: PreferencesSection, cx: &mut Context<Self>) {
        if self.section == section && self.selected_navigation_target.is_none() {
            return;
        }
        if section != PreferencesSection::Shortcuts {
            self.cancel_shortcut_capture(cx);
        }
        self.section = section;
        self.selected_navigation_target = None;
        cx.notify();
    }

    /// 根视图初始加载时读取当前完整快照，避免从另一处拼装第二份运行态设置。
    pub fn settings_snapshot(&self) -> Option<SettingsSnapshot> {
        self.settings.as_ref().map(|draft| draft.value().clone())
    }

    /// 兼容根视图按单字段初始化终端色盘的调用点。
    pub fn terminal_color_scheme(&self) -> Option<TerminalColorScheme> {
        self.settings
            .as_ref()
            .map(|draft| draft.value().terminal.color_scheme)
    }

    /// 在唯一应用 runtime 中加载 typed 设置快照；下游保留取消 token 与任务追踪语义。
    fn spawn_settings_load(&mut self, cx: &mut Context<Self>) {
        let generation = self.begin_settings_load();
        let service = self.preferences.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(|_| async move { service.load_settings().await })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_settings_load(result, generation, window, cx)
            });
        })
        .detach();
    }

    /// Profile 查询通过 application service 执行，Profile ID 与文件路径转换留在基础设施。
    fn spawn_profile_load(&mut self, cx: &mut Context<Self>) {
        let generation = self.begin_profile_load();
        let service = self.profiles.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(|_| async move { service.list_profiles().await })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_profile_load(result, generation, window, cx)
            });
        })
        .detach();
    }

    /// 每次配置读取递增代次并锁住对应表单，避免迟到结果覆盖更新后的用户输入。
    fn begin_settings_load(&mut self) -> u64 {
        self.settings_load_generation = self.settings_load_generation.wrapping_add(1).max(1);
        self.loading_settings = true;
        self.settings_load_generation
    }

    /// 显式列表读取与 watcher 回执互斥；被取代的监听读回在加载完成后重新核对。
    ///
    /// @author kongweiguang
    fn begin_profile_load(&mut self) -> u64 {
        self.profiles_watch_pending |= self.profiles_watch_in_flight;
        self.profiles_watch_in_flight = false;
        self.profiles_watch_generation = self.profiles_watch_generation.wrapping_add(1).max(1);
        self.profiles_load_generation = self.profiles_load_generation.wrapping_add(1).max(1);
        self.loading_profiles = true;
        self.profiles_load_generation
    }

    /// 完成首次加载或显式读回后只尝试可安全提交的末意图；失败 fence 不因读回 Ready 自动解除。
    ///
    /// @author kongweiguang
    fn finish_settings_load(
        &mut self,
        result: Result<VersionedSettings, AppError>,
        generation: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if generation != self.settings_load_generation {
            return;
        }
        let reloading = self.settings.is_some();
        self.loading_settings = false;
        match result {
            Ok(mut versioned) => {
                Self::resolve_legacy_terminal_theme(&mut versioned.settings, window, cx);
                let previous_theme_mode = self.theme_mode;
                let loaded_theme_mode = versioned.settings.theme_mode;
                let loaded_settings = versioned.settings.clone();
                self.install_settings(versioned, window, cx);
                self.settings_error = None;
                self.status = None;
                self.settings_success_status = reloading.then_some(SettingsSuccessStatus::Reloaded);
                if previous_theme_mode != loaded_theme_mode {
                    self.set_theme_mode(loaded_theme_mode, cx);
                }
                cx.emit(PreferencesEvent::SettingsLoaded(loaded_settings));
                self.flush_pending_workbench_layout(cx);
            }
            Err(error) => {
                self.settings_error = Some(localized_error(&error));
                self.status = None;
                self.settings_success_status = None;
            }
        }
        cx.notify();
    }

    /// 將應用返回的值和 revision 組成唯一草稿基線，防止異步請求使用过期设置覆盖后来编辑。
    ///
    /// @author kongweiguang
    fn install_settings(
        &mut self,
        mut versioned: VersionedSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        Self::resolve_legacy_terminal_theme(&mut versioned.settings, window, cx);
        let snapshot = DraftSnapshot {
            value: versioned.settings,
            revision: versioned.revision,
        };
        self.settings_autosave.reset();
        self.sync_settings_inputs(&snapshot.value, window, cx);
        self.mcp_settings.sync(&snapshot.value.mcp, window, cx);
        self.theme_mode = snapshot.value.theme_mode;
        self.settings_input_errors.clear();
        self.shortcut_field_errors.clear();
        self.reconcile_keyword_editor(
            &snapshot.value.terminal.keyword_highlights.rules,
            window,
            cx,
        );
        self.settings = Some(ConfigDraft::new(snapshot));
        // 读取与外部重载也经过同一 GPUI parser 诊断，避免只在用户编辑后才暴露无效配置。
        self.refresh_settings_issues();
    }

    /// 旧浅深配置只在建立 CAS 草稿前按真实外观归并；读取不会写磁盘，后续保存沿用同一主题身份。
    ///
    /// @author kongweiguang
    fn resolve_legacy_terminal_theme(
        settings: &mut SettingsSnapshot,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        super::legacy_terminal_theme::resolve_legacy_terminal_theme(
            settings,
            window.appearance(),
            cx,
        );
    }

    /// 列表读回以稳定 ID 核对未知写入；不匹配时保留草稿，不能把任意列表快照当作成功。
    /// 默认项与删除操作只需确认当前列表事实，且多个默认标记时继续阻止事务。
    ///
    /// @author kongweiguang
    fn finish_profile_load(
        &mut self,
        result: Result<Vec<VersionedProfile>, AppError>,
        generation: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if generation != self.profiles_load_generation {
            return;
        }
        self.loading_profiles = false;
        match result {
            Ok(rows) => {
                self.profile_list_error = None;
                if self.profile_default_outcome_unknown {
                    if rows.iter().filter(|row| row.profile.is_default).count() > 1 {
                        self.profile_rows = rows;
                        self.profile_list_error = Some(crate::locale::text(
                            "读回发现多个默认 Profile；请修复配置后再次读取。",
                            "The readback contains multiple default profiles. Repair the configuration and reload.",
                        ).to_string());
                        self.resume_deferred_profile_revision_check(cx);
                        cx.notify();
                        return;
                    }
                    self.profile_default_outcome_unknown = false;
                    cx.emit(PreferencesEvent::ProfilesChanged);
                    self.status = Some(crate::locale::text(
                        "已核对当前默认项，原操作未被重试。",
                        "The current default was verified; the original operation was not retried.",
                    ).to_string());
                } else if self.profile_delete_outcome_unknown {
                    self.profile_delete_outcome_unknown = false;
                    cx.emit(PreferencesEvent::ProfilesChanged);
                    self.status = Some(
                        crate::locale::text(
                            "已核对 Profile 列表，原删除操作未被重试。",
                            "The profile list was verified; the original delete was not retried.",
                        )
                        .to_string(),
                    );
                } else if let Some(submitted) = self.profile_draft.as_ref().and_then(|draft| {
                    (draft.status() == super::DraftStatus::OutcomeUnknown)
                        .then(|| draft.pending_save_value().cloned())
                        .flatten()
                }) {
                    let readback = submitted
                        .id
                        .as_deref()
                        .and_then(|id| rows.iter().find(|row| row.profile.id == id).cloned());
                    if readback
                        .as_ref()
                        .is_some_and(|row| profile_to_draft(row.profile.clone()) == submitted)
                    {
                        self.selected_profile_id = submitted.id;
                        cx.emit(PreferencesEvent::ProfilesChanged);
                        self.profile_unknown_readback = None;
                        self.status = Some(
                            crate::locale::text(
                                "已核对并确认 Profile 保存结果。",
                                "The profile save was verified.",
                            )
                            .to_string(),
                        );
                    } else {
                        self.profile_rows = rows;
                        self.profile_unknown_readback = Some(readback);
                        self.profile_list_error = Some(crate::locale::text(
                            "读回内容与提交内容不同；请选择采用当前版本或保留草稿继续。",
                            "The readback differs from the submitted profile. Use the current version or keep the draft.",
                        ).to_string());
                        self.resume_deferred_profile_revision_check(cx);
                        cx.notify();
                        return;
                    }
                }
                let requested = self.selected_profile_id.clone();
                let selected = requested
                    .as_deref()
                    .and_then(|id| rows.iter().find(|row| row.profile.id == id))
                    .or_else(|| rows.iter().find(|row| row.profile.is_default))
                    .or_else(|| rows.first())
                    .cloned();
                self.profile_rows = rows;
                if let Some(profile) = selected {
                    self.install_profile(profile, window, cx);
                } else {
                    self.selected_profile_id = None;
                    self.profile_draft = None;
                    self.selected_profile_is_default = false;
                }
            }
            Err(error) => self.profile_list_error = Some(localized_error(&error)),
        }
        self.resume_deferred_profile_revision_check(cx);
        cx.notify();
    }

    /// 从领域 Profile 建立表单草稿并携带仓储 revision，后续写入严格走 CAS。
    fn install_profile(
        &mut self,
        versioned: VersionedProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = versioned.profile.id.clone();
        let is_default = versioned.profile.is_default;
        let revision = versioned.revision;
        let draft = profile_to_draft(versioned.profile);
        self.selected_profile_id = Some(id);
        self.selected_profile_is_default = is_default;
        self.reset_profile_form(draft, Some(revision), is_default, window, cx);
    }

    /// 在读取回执中同步输入、选择和滑块，避免在 render 内改实体或让隐藏窗口等待呈现后才获得初始值。
    /// InputState.set_value 不发 Change；Select/Slider 的已确认值同步共用本次快照。
    fn sync_settings_inputs(
        &self,
        settings: &SettingsSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.interface_language_select.update(cx, |select, cx| {
            select.set_selected_value(&settings.interface_language, window, cx);
        });
        for (slider, percentage, minimum) in [
            (
                &self.background_opacity_slider,
                settings.background_opacity,
                0.,
            ),
            (&self.window_opacity_slider, settings.window_opacity, 35.),
        ] {
            let next = f32::from(percentage).clamp(minimum, 100.);
            if slider.read(cx).value().end().round() != next {
                slider.update(cx, |state, cx| state.set_value(next, window, cx));
            }
        }
        self.suggestion_selects
            .sync_from_settings(settings, window, cx);
        set_input(
            &self.settings_inputs.font_family,
            settings.terminal.font_family.clone(),
            window,
            cx,
        );
        set_input(
            &self.settings_inputs.font_size,
            settings.terminal.font_size.to_string(),
            window,
            cx,
        );
        set_input(
            &self.settings_inputs.line_height,
            settings.terminal.line_height.to_string(),
            window,
            cx,
        );
        set_input(
            &self.settings_inputs.scrollback,
            settings.terminal.scrollback.to_string(),
            window,
            cx,
        );
        set_input(
            &self.settings_inputs.background_path,
            settings.background_image_path.clone(),
            window,
            cx,
        );
        set_input(
            &self.settings_inputs.background_opacity,
            settings.background_opacity.to_string(),
            window,
            cx,
        );
        set_input(
            &self.settings_inputs.window_opacity,
            settings.window_opacity.to_string(),
            window,
            cx,
        );
        set_input(
            &self.settings_inputs.sftp_global_transfers,
            settings.sftp.global_transfers.to_string(),
            window,
            cx,
        );
        set_input(
            &self.settings_inputs.sftp_host_transfers,
            settings.sftp.host_transfers.to_string(),
            window,
            cx,
        );
        set_input(
            &self.settings_inputs.sftp_pipeline_depth,
            settings.sftp.pipeline_depth.to_string(),
            window,
            cx,
        );
        set_input(
            &self.settings_inputs.sftp_packet_bytes,
            (f64::from(settings.sftp.packet_bytes) / 1_048_576.).to_string(),
            window,
            cx,
        );
        set_input(
            &self.settings_inputs.sftp_idle_timeout,
            settings.sftp.idle_timeout_seconds.to_string(),
            window,
            cx,
        );
        set_input(
            &self.settings_inputs.notification_min_duration,
            (f64::from(settings.desktop_notifications.min_duration_ms) / 1000.).to_string(),
            window,
            cx,
        );
        set_input(
            &self.settings_inputs.notification_throttle,
            (f64::from(settings.desktop_notifications.throttle_ms) / 1000.).to_string(),
            window,
            cx,
        );
    }

    /// 根据 Profile typed 草稿设置输入值；环境条目按一行一个 KEY=VALUE 显示。
    fn sync_profile_inputs(
        &self,
        draft: &ProfileDraft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        set_input(&self.profile_inputs.name, draft.name.clone(), window, cx);
        set_input(&self.profile_inputs.shell, draft.shell.clone(), window, cx);
        set_input(
            &self.profile_inputs.working_directory,
            draft.cwd.clone().unwrap_or_default(),
            window,
            cx,
        );
        set_textarea(
            &self.profile_inputs.arguments,
            draft.args.join("\n"),
            window,
            cx,
        );
        set_textarea(
            &self.profile_inputs.environment,
            format_environment(&draft.env),
            window,
            cx,
        );
    }

    /// 清理 Profile 字段错误并创建一个不可变基线，避免切换控件时累积旧表单状态。
    fn reset_profile_form(
        &mut self,
        value: ProfileDraft,
        revision: Option<ConfigRevision>,
        is_default: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_profile_inputs(&value, window, cx);
        self.profile_draft = Some(ConfigDraft::new(DraftSnapshot { value, revision }));
        self.selected_profile_is_default = is_default;
        self.profile_input_errors.clear();
        self.profile_issues.clear();
    }
}

impl EventEmitter<PreferencesEvent> for PreferencesPanel {}

/// 把纯字符串行转换为环境变量 map；重复 key/缺少分隔符均提示用户显式修正。
pub(super) fn parse_environment(source: &str) -> Result<BTreeMap<String, String>, String> {
    let mut values = BTreeMap::new();
    for (index, raw_line) in source.lines().enumerate() {
        if raw_line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = raw_line.split_once('=') else {
            return Err(crate::locale::text(
                format!("第 {} 行需要使用 KEY=VALUE 格式。", index + 1),
                format!("Line {} must use KEY=VALUE format.", index + 1),
            )
            .to_string());
        };
        if key.trim().is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0')
        {
            return Err(crate::locale::text(
                format!("第 {} 行包含无效环境变量名或不可表示字符。", index + 1),
                format!("Line {} contains an invalid environment variable name or unsupported character.", index + 1),
            ).to_string());
        }
        if values
            .insert(key.trim().to_owned(), value.to_owned())
            .is_some()
        {
            return Err(crate::locale::text(
                format!("第 {} 行重复定义环境变量。", index + 1),
                format!(
                    "Line {} defines the same environment variable more than once.",
                    index + 1
                ),
            )
            .to_string());
        }
    }
    Ok(values)
}

/// 只读恢复文案使用固定原因类别和相对路径，不展示绝对目录或配置原文。
pub(super) fn recovery_issue_label(kind: ConfigurationIssueKind) -> gpui_kit::SharedString {
    match kind {
        ConfigurationIssueKind::MalformedToml => {
            crate::locale::text("TOML 格式错误", "Invalid TOML")
        }
        ConfigurationIssueKind::UnsupportedSchema => {
            crate::locale::text("配置版本暂不支持", "Unsupported configuration version")
        }
        ConfigurationIssueKind::Unreadable => {
            crate::locale::text("文件无法读取", "File could not be read")
        }
    }
}

/// 序列化环境行只用于显示，底层值仍由 application typed profile API 保存。
fn format_environment(environment: &BTreeMap<String, String>) -> String {
    environment
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Profile 领域实体映射成输入草稿，默认状态和时间戳由服务端单独管理。
fn profile_to_draft(profile: TerminalProfile) -> ProfileDraft {
    ProfileDraft {
        id: Some(profile.id),
        name: profile.name,
        shell: profile.shell,
        args: profile.args,
        cwd: profile.cwd,
        env: profile.env,
        sidebar_group_id: profile.sidebar_group_id,
        sort_order: profile.sort_order,
    }
}
