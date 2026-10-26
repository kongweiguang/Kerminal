//! 文件化设置与配置文档用例端口。
//!
//! @author kongweiguang

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use kerminal_domain::preferences::{
    ConfigDocument, ConfigDocumentPath, ConfigRevision, ConfigurationRecoveryState, TerminalProfile,
};
use kerminal_domain::sessions::LocalSessionTarget;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    AppError, AppResult,
    local_shells::{LocalShellCandidate, LocalShellDiscovery, MAX_LOCAL_SHELL_CANDIDATES},
};

pub mod settings;
pub use settings::{
    AgentLauncherSettings, BackgroundFit, CustomAgentDefinition, DesktopNotificationSettings,
    ExternalLaunchSettings, ExternalLaunchToolSetting, InterfaceDensity, InterfaceLanguage,
    KeybindingPreference, KeybindingScope, SettingsSnapshot, SftpPerformanceSettings,
    TerminalColorScheme, TerminalCommandSuggestionMenuShortcut,
    TerminalCommandSuggestionPresentation, TerminalCommandSuggestionRemoteRefresh,
    TerminalCursorStyle, TerminalFontWeight, TerminalInlineSuggestionAcceptKey,
    TerminalInlineSuggestionProviderSettings, TerminalInlineSuggestionSettings,
    TerminalKeywordHighlightColorPair, TerminalKeywordHighlightCustomColors,
    TerminalKeywordHighlightMatchMode, TerminalKeywordHighlightRule,
    TerminalKeywordHighlightSettings, TerminalKeywordHighlightStyle, TerminalRightClickBehavior,
    TerminalSettings, ThemeMode, ToolRailPanelPlacement, ToolRailSettings, ToolRailToolId,
    VersionedSettings, WorkbenchLayoutSettings,
};

/// 配置根、MCP 配置指南和本地文档共用的规则正文，避免多个入口维护分叉副本。
pub const CONFIG_GUIDE: &str = include_str!("../resources/config-guide.md");

/// 配置指南的内容修订号；生成文件用它和正文摘要区分受管旧版与用户自定义文档。
pub const CONFIG_GUIDE_REVISION: u32 = 10;

/// 启动时的指南状态，供 UI 明确告知应用已更新或保留了用户编辑内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum ConfigurationGuideStatus {
    Created,
    Current,
    Updated { previous_revision: u32 },
    UserModified,
    FutureRevision { revision: u32 },
    UpdateDeferred,
    Unavailable,
}

impl ConfigurationGuideStatus {
    /// 仅对需要用户知情的保留、升级或写入失败状态显示提示。
    pub const fn needs_notice(self) -> bool {
        !matches!(self, Self::Created | Self::Current)
    }
}

/// 设置用例只依赖 typed 仓储；磁盘格式转换与 CAS 细节由基础设施适配器承担。
#[async_trait]
pub trait SettingsRepository: Send + Sync + 'static {
    /// 读取设置投影及其原始文档 revision；缺失文档返回安全默认值和 None revision。
    async fn load_settings(&self) -> AppResult<VersionedSettings>;

    /// 按调用方读取的 revision 更新设置；实现必须在同一配置仓储中执行 CAS。
    async fn save_settings(
        &self,
        settings: SettingsSnapshot,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings>;
}

/// 文件仓储提供原始文档 CAS 与 typed 设置端口；UI 与 MCP 不直接解读磁盘格式。
#[async_trait]
pub trait ConfigurationRepository: SettingsRepository + Send + Sync + 'static {
    /// 返回进程启动时确定的恢复态，禁止不同调用方自行推断是否可写。
    fn recovery_state(&self) -> ConfigurationRecoveryState;

    /// 提供生成指南状态，不把用户自定义文件误认为可覆盖的应用配置。
    fn configuration_guide_status(&self) -> ConfigurationGuideStatus {
        ConfigurationGuideStatus::Current
    }

    /// 读取已知配置文档；文件缺失以 None 表示，不把缺失默认为可写的磁盘默认值。
    async fn read(&self, path: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>>;

    /// 按打开时 revision 原子保存；None 只表示明确创建，不可用于覆盖既有文件。
    async fn write(
        &self,
        path: ConfigDocumentPath,
        source: String,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ConfigRevision>;
}

/// Profile persistence port; implementations parse supported schema while retaining raw TOML separately.
#[async_trait]
pub trait ProfileRepository: Send + Sync + 'static {
    /// 返回现行 profile 记录；损坏文件已由启动恢复状态定位，不会被自动改写或静默修复。
    async fn list_profiles(&self) -> AppResult<Vec<TerminalProfile>>;

    /// 返回可编辑 profile 与原始文件 revision；实现必须保留 schema 未知字段和 CAS revision。
    async fn list_versioned_profiles(&self) -> AppResult<Vec<VersionedProfile>>;

    /// 按不透明 profile ID 读取单份可编辑快照，不在应用层拼接磁盘路径。
    async fn get_profile(&self, id: &str) -> AppResult<Option<VersionedProfile>>;

    /// 创建或更新一份 profile；None revision 仅允许新建，更新必须提交打开时 revision。
    async fn save_profile(
        &self,
        draft: ProfileDraft,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedProfile>;

    /// 按打开时 revision 删除 profile；过期 revision 返回冲突，不影响其它 profile。
    async fn delete_profile(&self, id: &str, expected_revision: ConfigRevision) -> AppResult<bool>;

    /// 在一次可恢复事务中切换唯一默认 profile，并校验调用方列出的全部 profile revision。
    async fn set_default_profile(
        &self,
        id: &str,
        expected_revisions: Vec<ProfileRevision>,
    ) -> AppResult<VersionedProfile>;
}

/// 平台默认 shell 由 infrastructure 解析，避免 GPUI/UI 知道系统环境变量与回退顺序。
pub trait DefaultLocalShellProvider: Send + Sync + 'static {
    /// 在没有显式默认 profile 时解析本地可启动 shell。
    fn resolve_default(&self) -> AppResult<LocalSessionTarget>;
}

/// 带磁盘 CAS token 的 profile 快照；编辑器不接触 TOML 的未知字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionedProfile {
    pub profile: TerminalProfile,
    pub revision: ConfigRevision,
}

/// Profile 表单字段；ID 可省略以由应用生成，默认状态由独立事务操作切换。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileDraft {
    pub id: Option<String>,
    pub name: String,
    pub shell: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub sidebar_group_id: Option<String>,
    pub sort_order: i64,
}

/// 默认切换时用于整体 CAS 的 profile ID 与文件 revision。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRevision {
    pub profile_id: String,
    pub revision: ConfigRevision,
}

/// 配置 validator 支持的范围；范围解析集中在 application，供 UI 与 MCP 共用。
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationValidationScope {
    All,
    Settings,
    Profiles,
    Hosts,
    Snippets,
    Workflows,
}

impl ConfigurationValidationScope {
    /// 省略或空白范围沿用旧 MCP 契约，解析为全量只读校验。
    pub fn parse(value: Option<&str>) -> AppResult<Self> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("all") => Ok(Self::All),
            Some("settings") => Ok(Self::Settings),
            Some("profiles") => Ok(Self::Profiles),
            Some("hosts") => Ok(Self::Hosts),
            Some("snippets") => Ok(Self::Snippets),
            Some("workflows") => Ok(Self::Workflows),
            Some(_) => Err(AppError::invalid("配置校验 scope 无效")),
        }
    }

    /// 返回冻结 MCP 结果所使用的稳定小写 scope token。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Settings => "settings",
            Self::Profiles => "profiles",
            Self::Hosts => "hosts",
            Self::Snippets => "snippets",
            Self::Workflows => "workflows",
        }
    }

    /// 判断当前请求是否包含单个文件化配置域。
    pub const fn includes(self, scope: Self) -> bool {
        matches!(
            (self, scope),
            (Self::All, _)
                | (Self::Settings, Self::Settings)
                | (Self::Profiles, Self::Profiles)
                | (Self::Hosts, Self::Hosts)
                | (Self::Snippets, Self::Snippets)
                | (Self::Workflows, Self::Workflows)
        )
    }
}

/// 校验已安全枚举到的配置项；path 固定为相对配置根路径。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationValidationCheck {
    pub scope: ConfigurationValidationScope,
    pub path: String,
    pub message: String,
}

/// 面向工具与设置页面的诊断级别；目前规则只产生错误，保留 warning 扩展点。
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigurationValidationSeverity {
    Error,
    Warning,
}

/// 配置问题只含安全说明与相对路径，不携带原始 TOML 值或凭据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationValidationDiagnostic {
    pub severity: ConfigurationValidationSeverity,
    pub scope: ConfigurationValidationScope,
    pub path: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<String>,
}

/// validator 的冻结输出形状；validation failures 是正常结果而非 transport error。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationValidationReport {
    pub valid: bool,
    pub scope: ConfigurationValidationScope,
    pub error_count: usize,
    pub warning_count: usize,
    pub checked: Vec<ConfigurationValidationCheck>,
    pub diagnostics: Vec<ConfigurationValidationDiagnostic>,
}

impl ConfigurationValidationReport {
    /// 初始化空报告；valid 与计数由记录方法共同维护，避免 MCP 层重算结果。
    pub fn new(scope: ConfigurationValidationScope) -> Self {
        Self {
            valid: true,
            scope,
            error_count: 0,
            warning_count: 0,
            checked: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    /// 记录一个只读成功检查项，path 必须来自仓储的固定白名单或安全目录扫描。
    pub fn record_checked(
        &mut self,
        scope: ConfigurationValidationScope,
        path: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.checked.push(ConfigurationValidationCheck {
            scope,
            path: path.into(),
            message: message.into(),
        });
    }

    /// 按诊断自身级别更新报告计数，避免调用方重复传递或误配同一组字段。
    ///
    /// @author kongweiguang
    pub fn record(&mut self, diagnostic: ConfigurationValidationDiagnostic) {
        match diagnostic.severity {
            ConfigurationValidationSeverity::Error => {
                self.valid = false;
                self.error_count += 1;
            }
            ConfigurationValidationSeverity::Warning => self.warning_count += 1,
        }
        self.diagnostics.push(diagnostic);
    }
}

/// 只读校验 port；adapter 负责读取与使用文件格式 validator，不写入任何配置。
#[async_trait]
pub trait ConfigurationValidationRepository: Send + Sync + 'static {
    /// 返回成功检查项和可修复诊断；单个坏文档仍应作为 report diagnostics 返回。
    async fn validate_configuration(
        &self,
        scope: ConfigurationValidationScope,
    ) -> AppResult<ConfigurationValidationReport>;
}

/// 配置校验 application service；只转发显式范围并保留正常的 invalid report。
#[derive(Clone)]
pub struct ConfigurationValidationService {
    repository: Arc<dyn ConfigurationValidationRepository>,
}

impl ConfigurationValidationService {
    /// 通过共享的只读仓储构造，避免 MCP 自行读取配置或复刻 TOML 规则。
    pub fn new(repository: Arc<dyn ConfigurationValidationRepository>) -> Self {
        Self { repository }
    }

    /// 执行文件化配置只读校验；损坏配置被结果报告，不会被转换成传输失败。
    pub async fn validate(
        &self,
        scope: ConfigurationValidationScope,
    ) -> AppResult<ConfigurationValidationReport> {
        self.repository.validate_configuration(scope).await
    }
}

/// UI 与 MCP 共用的轻量用例句柄，保持配置能力受 repository 恢复门禁约束。
#[derive(Clone)]
pub struct PreferencesService {
    repository: Arc<dyn ConfigurationRepository>,
}

impl PreferencesService {
    /// 注入同一进程唯一的配置仓储，避免 UI 和 MCP 分别建立不同写入策略。
    pub fn new(repository: Arc<dyn ConfigurationRepository>) -> Self {
        Self { repository }
    }

    /// 暴露可安全展示的恢复态，不包含配置根绝对路径或原始内容。
    pub fn recovery_state(&self) -> ConfigurationRecoveryState {
        self.repository.recovery_state()
    }

    /// 返回指南播种或升级状态，让桌面提示自定义文件和待处理升级而不影响配置写入门禁。
    pub fn configuration_guide_status(&self) -> ConfigurationGuideStatus {
        self.repository.configuration_guide_status()
    }

    /// 读取一份文件配置及其 CAS revision，缺失配置由调用方展示首次配置体验。
    pub async fn read(&self, path: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
        self.repository.read(path).await
    }

    /// 拒绝空内容以免编辑器误保存空文件覆盖有效配置；更细领域校验由文件 validator 执行。
    pub async fn write(
        &self,
        path: ConfigDocumentPath,
        source: String,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ConfigRevision> {
        if source.trim().is_empty() {
            return Err(AppError::invalid("配置文档不能为空"));
        }
        self.repository.write(path, source, expected_revision).await
    }

    /// 从 typed 设置端口读取应用投影，并再次校验跨边界返回值。
    pub async fn load_settings(&self) -> AppResult<VersionedSettings> {
        let mut settings = self.repository.load_settings().await?;
        settings.settings = settings.settings.validated()?;
        Ok(settings)
    }

    /// 先应用层校验，再把 typed 投影与 CAS token 交给配置仓储合并磁盘文档。
    pub async fn save_settings(
        &self,
        settings: SettingsSnapshot,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        let settings = settings.validated()?;
        self.repository
            .save_settings(settings, expected_revision)
            .await
    }
}

/// 默认终端目标用例，统一解释 profile 的 `is_default` 并委托平台 shell 回退。
#[derive(Clone)]
pub struct ProfileService {
    repository: Arc<dyn ProfileRepository>,
    default_shell: Arc<dyn DefaultLocalShellProvider>,
    shell_discovery: Option<Arc<dyn LocalShellDiscovery>>,
}

impl ProfileService {
    /// 组合同一份配置 repository 与平台 shell resolver，调用方不再自行读 TOML 或猜操作系统。
    pub fn new(
        repository: Arc<dyn ProfileRepository>,
        default_shell: Arc<dyn DefaultLocalShellProvider>,
    ) -> Self {
        Self {
            repository,
            default_shell,
            shell_discovery: None,
        }
    }

    /// 通过独立发现端口扩展 Profile 表单能力，同时让既有配置与 MCP 构造方式保持兼容。
    pub fn with_shell_discovery(mut self, discovery: Arc<dyn LocalShellDiscovery>) -> Self {
        self.shell_discovery = Some(discovery);
        self
    }

    /// 在 blocking worker 中执行有界的文件元数据探测，取消后不把迟到候选交给 UI。
    pub async fn discover_local_shells(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<LocalShellCandidate>> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("本地 Shell 发现已取消"));
        }
        let discovery =
            self.shell_discovery.as_ref().cloned().ok_or_else(|| {
                AppError::unsupported("本地 Shell 发现不可用，可手动填写程序路径")
            })?;
        let worker_cancellation = cancellation.clone();
        let discovery_result =
            tokio::task::spawn_blocking(move || discovery.discover(&worker_cancellation))
                .await
                .map_err(|source| {
                    AppError::infrastructure_with_source("本地 Shell 发现后台任务失败", source)
                })?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("本地 Shell 发现已取消"));
        }
        let candidates = discovery_result?;
        if candidates.len() > MAX_LOCAL_SHELL_CANDIDATES {
            return Err(AppError::invalid("本地 Shell 候选数量超出限制"));
        }
        let mut ids = std::collections::HashSet::with_capacity(candidates.len());
        if candidates
            .iter()
            .any(|candidate| !ids.insert(candidate.id().to_owned()))
        {
            return Err(AppError::invalid("本地 Shell 候选身份重复"));
        }
        Ok(candidates)
    }

    /// 使用唯一默认 profile；多个 default 属于配置冲突，零个才回退平台 shell。
    pub async fn default_local_session_target(&self) -> AppResult<LocalSessionTarget> {
        let profiles = self.repository.list_profiles().await?;
        let mut defaults = profiles.into_iter().filter(|profile| profile.is_default);
        match (defaults.next(), defaults.next()) {
            (Some(_), Some(_)) => Err(AppError::conflict("多个本地 profile 被标记为默认")),
            (Some(profile), None) => Ok(profile_target(profile)),
            (None, _) => self.default_shell.resolve_default(),
        }
    }

    /// 返回含 CAS token 的 profile 列表，供表单进行可靠保存和默认切换。
    pub async fn list_profiles(&self) -> AppResult<Vec<VersionedProfile>> {
        self.repository.list_versioned_profiles().await
    }

    /// 按 ID 读取 profile，文件路径与 ID 的安全映射留在基础设施仓储中。
    pub async fn get_profile(&self, id: &str) -> AppResult<Option<VersionedProfile>> {
        self.repository.get_profile(id).await
    }

    /// 按 workspace 中保存的 profile ID 解析本地启动目标，避免恢复流程重复解释 TOML 字段。
    pub async fn local_session_target(&self, id: &str) -> AppResult<Option<LocalSessionTarget>> {
        Ok(self
            .repository
            .get_profile(id)
            .await?
            .map(|versioned| profile_target(versioned.profile)))
    }

    /// 校验草稿后创建或更新；None revision 新建，有 revision 才能更新。
    pub async fn save_profile(
        &self,
        draft: ProfileDraft,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedProfile> {
        validate_profile_draft(&draft)?;
        self.repository.save_profile(draft, expected_revision).await
    }

    /// 只允许用打开时的 revision 删除，避免删掉用户或外部编辑器刚改过的 profile。
    pub async fn delete_profile(
        &self,
        id: &str,
        expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        self.repository.delete_profile(id, expected_revision).await
    }

    /// 依据列表快照一次切换默认项；实现必须以事务或可恢复 journal 保证唯一性。
    pub async fn set_default_profile(
        &self,
        id: &str,
        expected_revisions: Vec<ProfileRevision>,
    ) -> AppResult<VersionedProfile> {
        self.repository
            .set_default_profile(id, expected_revisions)
            .await
    }
}

/// 把 profile 的字符串字段转换为 transport 的纯数据目标，保留参数、工作目录和环境覆盖。
fn profile_target(profile: TerminalProfile) -> LocalSessionTarget {
    LocalSessionTarget {
        program: OsString::from(profile.shell),
        args: profile.args.into_iter().map(OsString::from).collect(),
        working_directory: profile.cwd.map(PathBuf::from),
        environment: profile
            .env
            .into_iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect::<BTreeMap<_, _>>(),
    }
}

/// 拒绝空白或过长 profile 字段，防止无意义配置和超大环境表进入文件。
fn validate_profile_draft(draft: &ProfileDraft) -> AppResult<()> {
    if draft.name.trim().is_empty() || draft.name.len() > 256 || draft.name.contains('\0') {
        return Err(AppError::invalid("profile 名称不能为空或过长"));
    }
    if draft.shell.trim().is_empty() || draft.shell.len() > 1024 || draft.shell.contains('\0') {
        return Err(AppError::invalid("profile shell 路径不能为空或过长"));
    }
    if draft
        .cwd
        .as_ref()
        .is_some_and(|cwd| cwd.len() > 4096 || cwd.contains('\0'))
        || draft
            .args
            .iter()
            .any(|argument| argument.len() > 16 * 1024 || argument.contains('\0'))
        || draft.env.len() > 256
        || draft.env.iter().any(|(key, value)| {
            key.is_empty()
                || key.len() > 1024
                || key.contains('\0')
                || key.contains('=')
                || value.len() > 16 * 1024
                || value.contains('\0')
        })
    {
        return Err(AppError::invalid("profile 参数或环境变量超出限制"));
    }
    Ok(())
}
