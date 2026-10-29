//! 偏好设置面板的用例协调、草稿状态与配置恢复边界。
//!
//! @author kongweiguang

use std::collections::BTreeMap;

use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, Focusable as _, SharedString, Subscription,
    Window,
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
    connections::management::{HostGroup, HostGroupSnapshot, HostManagementService},
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
#[path = "panel/profiles.rs"]
mod profiles;
#[path = "panel/protocol.rs"]
pub(super) mod protocol;
#[path = "panel/recovery.rs"]
mod recovery;
#[path = "panel/settings.rs"]
mod settings;
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

/// 本地 Profile 的分组选择值携带稳定 group ID，显示文字只用于搜索和辅助技术。
#[derive(Clone)]
pub(super) struct ProfileGroupOption {
    id: Option<String>,
    name: ProfileGroupName,
}

#[derive(Clone)]
pub(super) enum ProfileGroupName {
    Ungrouped,
    Missing(String),
    Named(SharedString),
}

impl SearchableListItem for ProfileGroupOption {
    type Value = Option<String>;

    /// 搜索结果展示可读分组名；通用选项在读取时本地化，避免切换语言重建 Select。
    ///
    /// @author kongweiguang
    fn title(&self) -> SharedString {
        match &self.name {
            ProfileGroupName::Ungrouped => crate::locale::text("未分组", "Ungrouped"),
            ProfileGroupName::Missing(id) => crate::locale::text(
                format!("已删除分组（{id}）"),
                format!("Missing group ({id})"),
            ),
            ProfileGroupName::Named(name) => name.clone(),
        }
    }

    /// Profile TOML 保存的是分组 ID，不把本地化显示名持久化。
    /// @author kongweiguang
    fn value(&self) -> &Self::Value {
        &self.id
    }
}

/// “未分组”和失效组名称按当前 locale 生成，真实分组按 HostManagementService 快照列出。
/// @author kongweiguang
pub(super) fn profile_group_options(
    groups: &[HostGroup],
    selected_group_id: Option<&str>,
) -> Vec<ProfileGroupOption> {
    let stale_group =
        selected_group_id.filter(|selected| !groups.iter().any(|group| group.id == *selected));
    let mut options = Vec::with_capacity(groups.len() + 1 + usize::from(stale_group.is_some()));
    options.push(ProfileGroupOption {
        id: None,
        name: ProfileGroupName::Ungrouped,
    });
    if let Some(id) = stale_group {
        options.push(ProfileGroupOption {
            id: Some(id.to_owned()),
            name: ProfileGroupName::Missing(id.to_owned()),
        });
    }
    options.extend(groups.iter().map(|group| ProfileGroupOption {
        id: Some(group.id.clone()),
        name: ProfileGroupName::Named(SharedString::from(group.name.clone())),
    }));
    options
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

/// 单个快捷键的三平台受控输入；Windows 主键与备用键同属一列，避免压缩平台列宽。
pub(super) struct ShortcutInputs {
    pub(super) action: String,
    pub(super) windows: Entity<InputState>,
    pub(super) windows_alternate: Entity<InputState>,
    pub(super) linux: Entity<InputState>,
    pub(super) mac: Entity<InputState>,
}

/// 平台快捷键字段区分 Windows 主键、备用键、Linux 与 macOS，保留 Option 的缺省/显式空值语义。
pub(super) enum ShortcutBindingField {
    Windows,
    WindowsAlternate,
    Linux,
    Mac,
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

/// settings 与 Profile 页面状态只保存 typed 快照和 opaque revision，文件访问由 application 负责。
pub struct PreferencesPanel {
    screen_color_sampling: color_sampling::ScreenColorSamplingState,
    settings_autosave: autosave::SettingsAutosaveState,
    pub(super) keyword_editor: terminal::KeywordEditorState,
    pub(super) suggestion_selects: terminal::SuggestionSelects,
    pub(super) connectivity: connectivity::ConnectivityState,
    pub(super) desktop_about: desktop_about::DesktopAboutState,
    pub(super) shortcut_platform: super::KeyboardPlatform,
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
    pub(super) profile_groups: Vec<HostGroup>,
    pub(super) profile_group_select: Entity<SelectState<Vec<ProfileGroupOption>>>,
    pub(super) _profile_group_subscription: Subscription,
    pub(super) profile_group_loading: bool,
    pub(super) profile_groups_load_started: bool,
    profile_groups_load_generation: u64,
    pub(super) profile_group_error: Option<String>,
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
    pub(super) settings_content_focus: gpui_kit::FocusHandle,
    pub(super) settings_close_focus: gpui_kit::FocusHandle,
    pub(super) terminal_renderer_open: bool,
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
    pub(super) shortcut_inputs: Vec<ShortcutInputs>,
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
    pub(super) pending_profile_group_id: Option<Option<String>>,
    pub(super) profile_list_error: Option<String>,
    pub(super) settings_error: Option<String>,
    // 订阅对象只用于保持 GPUI 输入监听存活；订阅本身不需要被主动读取。
    pub(super) _settings_subscriptions: Vec<Subscription>,
    pub(super) _profile_subscriptions: Vec<Subscription>,
    pub(super) shortcut_subscriptions: Vec<Subscription>,
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
    /// 使用 composition root 注入的服务；输入实体保持原生焦点和 IME，快捷键平台在 macOS 默认选 Mac、其它桌面选 Windows。
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
        let connectivity =
            connectivity::ConnectivityState::new(mcp_runtime, executor.clone(), window, cx);
        let (settings_inputs, settings_subscriptions) = Self::create_settings_inputs(window, cx);
        Self::configure_settings_number_inputs(&settings_inputs, window, cx);
        let (profile_inputs, profile_subscriptions) = Self::create_profile_inputs(window, cx);
        let profile_group_select =
            cx.new(|cx| SelectState::new(profile_group_options(&[], None), None, window, cx));
        let profile_group_subscription = cx.subscribe(
            &profile_group_select,
            |panel, _, event: &SelectEvent<Vec<ProfileGroupOption>>, cx| {
                if let SelectEvent::Confirm(Some(group_id)) = event {
                    panel.update_profile_group(group_id.clone(), cx);
                }
            },
        );
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
            shortcut_platform: if cfg!(target_os = "macos") {
                super::KeyboardPlatform::Mac
            } else {
                super::KeyboardPlatform::Windows
            },
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
            profile_groups: Vec::new(),
            profile_group_select,
            _profile_group_subscription: profile_group_subscription,
            profile_group_loading: false,
            profile_groups_load_started: false,
            profile_groups_load_generation: 0,
            profile_group_error: None,
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
            settings_content_focus: cx.focus_handle(),
            settings_close_focus: cx.focus_handle(),
            terminal_renderer_open: false,
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
            shortcut_inputs: Vec::new(),
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
            pending_profile_group_id: None,
            profile_list_error: None,
            settings_error: None,
            _settings_subscriptions: settings_subscriptions,
            _profile_subscriptions: profile_subscriptions,
            shortcut_subscriptions: Vec::new(),
            _agent_launcher_subscriptions: agent_launcher_subscriptions,
            custom_agent_name: String::new(),
            custom_agent_command: String::new(),
        };
        panel.ensure_loaded(window, cx);
        panel.load_integrations(cx);
        panel.watch_configuration_changes(cx);
        panel
    }

    /// 连接同一主机分组用例；Profile 只保存 group ID，不在偏好面板复制配置仓储。
    /// @author kongweiguang
    pub fn with_host_management_service(mut self, service: HostManagementService) -> Self {
        self.host_management = Some(service);
        self
    }

    /// 从 Profile 侧栏入口打开指定本地草稿，组 ID 原样进入现有 CAS 保存流程。
    /// @author kongweiguang
    pub fn begin_local_profile_editor(
        &mut self,
        group_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.section = PreferencesSection::Profiles;
        self.ensure_profile_groups_loaded(cx);
        self.request_profile_selection_in_group(None, Some(group_id), window, cx);
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

    /// main 无 autofocus 字段时聚焦首个关闭按钮，保持初始 Tab 顺序且隔离后层终端输入。
    ///
    /// @author kongweiguang
    pub(crate) fn focus_initial_control(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.settings_close_focus, cx);
    }

    /// 显式搜索动作聚焦搜索框；普通打开沿用 main 的关闭按钮初始焦点。
    ///
    /// @author kongweiguang
    pub fn focus_navigation_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.navigation_search.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    }

    /// 普通入口像 main 重新挂载设置正文；保留在途保存，并重新核实上次未确认的写入结果。
    ///
    /// @author kongweiguang
    pub(crate) fn begin_modal_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        self.terminal_renderer_open = false;
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

    /// 子控件先消费浮层 Escape；到达面板后撤销确认或关闭设置，与 main 遮罩语义一致。
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
            Ok(versioned) => {
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
        versioned: VersionedSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = DraftSnapshot {
            value: versioned.settings,
            revision: versioned.revision,
        };
        self.settings_autosave.reset();
        self.sync_settings_inputs(&snapshot.value, window, cx);
        self.theme_mode = snapshot.value.theme_mode;
        self.settings_input_errors.clear();
        self.rebuild_shortcut_inputs(&snapshot.value, window, cx);
        self.reconcile_keyword_editor(
            &snapshot.value.terminal.keyword_highlights.rules,
            window,
            cx,
        );
        self.settings = Some(ConfigDraft::new(snapshot));
        // 读取与外部重载也经过同一 GPUI parser 诊断，避免只在用户编辑后才暴露无效配置。
        self.refresh_settings_issues();
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

    /// Profile 分组选项来自主机目录的已验证快照；加载失败时保留旧快照并显式提示。
    /// @author kongweiguang
    pub(super) fn ensure_profile_groups_loaded(&mut self, cx: &mut Context<Self>) {
        if !self.profile_groups_load_started {
            self.profile_groups_load_started = true;
            self.reload_profile_groups(cx);
        }
    }

    /// 重新读取组目录并递增代次，迟到的旧结果不能覆盖 watcher 后发起的新查询。
    /// @author kongweiguang
    pub(super) fn reload_profile_groups(&mut self, cx: &mut Context<Self>) {
        let Some(service) = self.host_management.clone() else {
            self.profile_group_error = Some(
                crate::locale::text(
                    "主机分组服务不可用，Profile 只能保存为未分组。",
                    "Host groups are unavailable; profiles can only be saved ungrouped.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        self.profile_groups_load_started = true;
        self.profile_groups_load_generation =
            self.profile_groups_load_generation.wrapping_add(1).max(1);
        let generation = self.profile_groups_load_generation;
        self.profile_group_loading = true;
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.load_groups().await })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_profile_groups(result, generation, window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// 按最新组快照更新 Select，同时把失效引用作为显式选项保留，避免静默换组。
    /// @author kongweiguang
    fn finish_profile_groups(
        &mut self,
        result: Result<HostGroupSnapshot, AppError>,
        generation: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if generation != self.profile_groups_load_generation {
            return;
        }
        self.profile_group_loading = false;
        match result {
            Ok(snapshot) => {
                self.profile_groups = snapshot.groups;
                self.profile_group_error = None;
            }
            Err(error) => self.profile_group_error = Some(localized_error(&error)),
        }
        let selected_group_id = self
            .profile_draft
            .as_ref()
            .and_then(|draft| draft.value().sidebar_group_id.clone());
        let options = profile_group_options(&self.profile_groups, selected_group_id.as_deref());
        self.profile_group_select.update(cx, |select, cx| {
            select.set_items(options, window, cx);
            select.set_selected_value(&selected_group_id, window, cx);
        });
        if selected_group_id
            .as_deref()
            .is_some_and(|selected| !self.profile_groups.iter().any(|group| group.id == selected))
        {
            self.profile_group_error = Some(crate::locale::text(
                "当前 Profile 引用了已删除分组；请选择新分组或未分组后再保存。",
                "This profile refers to a deleted group. Choose another group or Ungrouped before saving.",
            ).to_string());
        }
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
        let selected_group_id = value.sidebar_group_id.clone();
        let options = profile_group_options(&self.profile_groups, selected_group_id.as_deref());
        self.profile_group_select.update(cx, |select, cx| {
            select.set_items(options, window, cx);
            select.set_selected_value(&selected_group_id, window, cx);
        });
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
