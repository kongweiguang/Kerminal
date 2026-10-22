//! Agent Launcher 选择解析和配置刷新契约。
//!
//! @author kongweiguang

use std::{ffi::OsString, sync::Arc};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult, PreferencesService, ProfileService,
    agents::{AgentLaunchResolver, resolve_agent_launcher},
    preferences::{
        AgentLauncherSettings, ConfigurationGuideStatus, ConfigurationRepository,
        CustomAgentDefinition, DefaultLocalShellProvider, ProfileDraft, ProfileRepository,
        ProfileRevision, SettingsRepository, SettingsSnapshot, VersionedProfile, VersionedSettings,
    },
};
use kerminal_domain::{
    agents::{AgentLauncherKey, AgentProvider},
    preferences::{
        ConfigDocument, ConfigDocumentPath, ConfigRevision, ConfigurationMode,
        ConfigurationRecoveryState, TerminalProfile,
    },
    sessions::LocalSessionTarget,
};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// 测试设置及其 revision 共用一把锁，模拟应用层 CAS 而不接触正式配置根。
#[derive(Default)]
struct MemorySettingsState {
    settings: Option<SettingsSnapshot>,
    revision: Option<ConfigRevision>,
    generation: u64,
    conflicting_settings: Option<SettingsSnapshot>,
}

/// 用内存配置仓储模拟桌面进程中的共享设置服务，保证测试不会读写正式数据根。
#[derive(Default)]
struct MemoryConfigurationRepository {
    settings: RwLock<MemorySettingsState>,
}

impl MemoryConfigurationRepository {
    /// 替换测试配置源并推进 revision，以模拟外部文件编辑后的新设置快照。
    async fn replace_settings(&self, settings: &SettingsSnapshot) {
        let mut state = self.settings.write().await;
        state.settings = Some(settings.clone());
        state.generation = state.generation.saturating_add(1);
        state.revision = Some(test_revision(state.generation));
    }

    /// 在下一次保存前提交一份并发设置，验证 CAS 不会覆盖刚刚完成的用户修改。
    async fn conflict_next_save_with(&self, settings: &SettingsSnapshot) {
        self.settings.write().await.conflicting_settings = Some(settings.clone());
    }

    /// 读取测试保存后的完整设置，确认选择用例没有覆盖其他用户字段。
    async fn saved_settings(&self) -> Option<SettingsSnapshot> {
        self.settings.read().await.settings.clone()
    }
}

#[async_trait]
impl SettingsRepository for MemoryConfigurationRepository {
    /// 返回直接注入的应用投影，测试不再借助 TOML 模拟持久化边界。
    async fn load_settings(&self) -> AppResult<VersionedSettings> {
        let state = self.settings.read().await;
        Ok(VersionedSettings {
            settings: state.settings.clone().unwrap_or_default(),
            revision: state.revision.clone(),
        })
    }

    /// 使用读取时的 revision 保存完整 typed 快照，模拟设置仓储的原子 CAS 边界。
    async fn save_settings(
        &self,
        settings: SettingsSnapshot,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        let mut state = self.settings.write().await;
        if let Some(concurrent_settings) = state.conflicting_settings.take() {
            state.generation = state.generation.saturating_add(1);
            state.settings = Some(concurrent_settings);
            state.revision = Some(test_revision(state.generation));
            return Err(AppError::conflict("test settings revision changed"));
        }
        if state.revision != expected_revision {
            return Err(AppError::conflict("test settings revision changed"));
        }
        state.generation = state.generation.saturating_add(1);
        state.settings = Some(settings.clone());
        state.revision = Some(test_revision(state.generation));
        Ok(VersionedSettings {
            settings,
            revision: state.revision.clone(),
        })
    }
}

/// 测试 revision 只用于比较先后，不模拟磁盘摘要或真实配置内容。
/// @author kongweiguang
fn test_revision(generation: u64) -> ConfigRevision {
    ConfigRevision::from_sha256(format!("agent-launcher-test-{generation}"))
}

#[async_trait]
impl ConfigurationRepository for MemoryConfigurationRepository {
    /// 测试配置始终可写，且不涉及正式配置目录或凭据恢复状态。
    fn recovery_state(&self) -> ConfigurationRecoveryState {
        ConfigurationRecoveryState {
            mode: ConfigurationMode::ReadWrite,
            issues: Vec::new(),
        }
    }

    /// 原文配置编辑不属于 launcher 测试范围，设置读取统一走 typed settings port。
    async fn read(&self, path: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
        let _ = path;
        Err(AppError::unsupported(
            "Agent launcher settings fake is typed-only",
        ))
    }

    /// 测试不会通过 PreferencesService 写配置；返回明确错误，避免假装持久化成功。
    async fn write(
        &self,
        _path: ConfigDocumentPath,
        _source: String,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ConfigRevision> {
        Err(AppError::unsupported(
            "memory repository is read-only in tests",
        ))
    }

    /// 测试仓储无配置指南播种行为，只用于满足可见恢复状态契约。
    fn configuration_guide_status(&self) -> ConfigurationGuideStatus {
        ConfigurationGuideStatus::Current
    }
}

#[async_trait]
impl ProfileRepository for MemoryConfigurationRepository {
    /// 测试仓储无显式 profile，让 shell 解析走单独注入的确定性平台 provider。
    async fn list_profiles(&self) -> AppResult<Vec<TerminalProfile>> {
        Ok(Vec::new())
    }

    /// Launcher 测试不编辑 profile，返回空 CAS 列表。
    async fn list_versioned_profiles(&self) -> AppResult<Vec<VersionedProfile>> {
        Ok(Vec::new())
    }

    /// Launcher 测试没有持久化 profile，按普通缺失处理。
    async fn get_profile(&self, _id: &str) -> AppResult<Option<VersionedProfile>> {
        Ok(None)
    }

    /// 测试中的配置来源是只读设置快照，不伪造 profile 保存成功。
    async fn save_profile(
        &self,
        _draft: ProfileDraft,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedProfile> {
        Err(AppError::unsupported(
            "launcher test profile store is read-only",
        ))
    }

    /// Launcher 测试不删除 profile。
    async fn delete_profile(
        &self,
        _id: &str,
        _expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        Err(AppError::unsupported(
            "launcher test profile store is read-only",
        ))
    }

    /// Launcher 测试不切换默认 profile。
    async fn set_default_profile(
        &self,
        _id: &str,
        _expected_revisions: Vec<ProfileRevision>,
    ) -> AppResult<VersionedProfile> {
        Err(AppError::unsupported(
            "launcher test profile store is read-only",
        ))
    }
}

#[cfg(windows)]
struct TestShellProvider {
    program: &'static str,
    args: Vec<OsString>,
}

#[cfg(windows)]
impl TestShellProvider {
    /// 把 Profile argv 固定在 fake 中，验证 Agent 仅变换启动副本而不修改普通终端配置。
    ///
    /// @author kongweiguang
    fn new(program: &'static str, args: &[&str]) -> Self {
        Self {
            program,
            args: args.iter().map(OsString::from).collect(),
        }
    }
}

#[cfg(windows)]
impl DefaultLocalShellProvider for TestShellProvider {
    /// 为 Windows 分支提供显式 shell 名称，避免测试探测或依赖机器安装状态。
    ///
    /// @author kongweiguang
    fn resolve_default(&self) -> AppResult<LocalSessionTarget> {
        Ok(LocalSessionTarget {
            program: OsString::from(self.program),
            args: self.args.clone(),
            working_directory: None,
            environment: Default::default(),
        })
    }
}

/// 构造包含一个已选择自定义定义的有效设置快照。
/// @author kongweiguang
fn selected_custom(id: &str, name: &str, command: &str) -> SettingsSnapshot {
    SettingsSnapshot {
        agent_launcher: AgentLauncherSettings {
            selected_agent_key: format!("custom:{id}"),
            custom_agents: vec![CustomAgentDefinition {
                id: id.to_owned(),
                name: name.to_owned(),
                command: command.to_owned(),
            }],
        },
        ..SettingsSnapshot::default()
    }
}

/// 保留各 CLI 的启动语义：Codex/Claude 使用完整权限参数，Pi 信任受管 session 的项目配置。
#[test]
fn builtin_launcher_preserves_provider_permission_modes() {
    let mut settings = SettingsSnapshot::default();
    settings.agent_launcher.selected_agent_key = "builtin:claude".to_owned();

    let resolved = resolve_agent_launcher(&settings).expect("builtin launcher resolves");

    assert_eq!(resolved.provider, AgentProvider::Claude);
    assert_eq!(resolved.launcher_key.as_str(), "builtin:claude");
    assert_eq!(resolved.command_label, "Claude Code");
    assert_eq!(resolved.program, "claude");
    assert_eq!(resolved.args, ["--dangerously-skip-permissions"]);

    settings.agent_launcher.selected_agent_key = "builtin:codex".to_owned();
    let codex = resolve_agent_launcher(&settings).expect("Codex launcher resolves");
    assert_eq!(
        codex.args,
        [
            "--dangerously-bypass-approvals-and-sandbox",
            "--dangerously-bypass-hook-trust"
        ]
    );

    settings.agent_launcher.selected_agent_key = "builtin:pi".to_owned();
    let pi = resolve_agent_launcher(&settings).expect("Pi launcher resolves");
    assert_eq!(pi.args, ["--approve"]);
}

/// Custom 命令只做安全 argv 分词，命令替换语法作为普通参数原样保留。
#[cfg(not(windows))]
#[test]
fn custom_launcher_parses_arguments_without_shell_evaluation() {
    let settings = selected_custom(
        "11111111-1111-4111-8111-111111111111",
        "Review Bot",
        r#"review-cli --workspace "project folder" $(touch marker)"#,
    );

    let resolved = resolve_agent_launcher(&settings).expect("custom launcher resolves");

    assert_eq!(resolved.provider, AgentProvider::Custom);
    assert_eq!(
        resolved.launcher_key.as_str(),
        "custom:11111111-1111-4111-8111-111111111111"
    );
    assert_eq!(resolved.command_label, "Review Bot");
    assert_eq!(resolved.program, "review-cli");
    assert_eq!(
        resolved.args,
        ["--workspace", "project folder", "$(touch", "marker)"]
    );
}

/// 坏引号和 NUL 必须显式失败，不能回退到 Codex 或另一个定义。
#[cfg(not(windows))]
#[test]
fn invalid_custom_command_does_not_fallback() {
    let unclosed = selected_custom(
        "11111111-1111-4111-8111-111111111111",
        "Review Bot",
        "review-cli \"unfinished",
    );
    let nul = selected_custom(
        "22222222-2222-4222-8222-222222222222",
        "Review Bot",
        "review-cli\0 --workspace",
    );

    assert!(resolve_agent_launcher(&unclosed).is_err());
    assert!(resolve_agent_launcher(&nul).is_err());
}

/// Agent PowerShell 派生 argv 会移除所有真实可匹配的 NoExit 缩写，同时保留普通 profile 开关和原始脚本文本。
///
/// @author kongweiguang
#[cfg(windows)]
#[tokio::test]
async fn windows_custom_launcher_exits_after_command_and_preserves_profile() {
    let repository = Arc::new(MemoryConfigurationRepository::default());
    let raw_command = "AGENT_MODE=review; review-cli | tee session.log";
    let profile_args = [
        "-NoE",
        "-NOEX",
        "-NoExI",
        "-noexit",
        "/NoE",
        "--NoEx",
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
    ];
    repository
        .replace_settings(&selected_custom(
            "11111111-1111-4111-8111-111111111111",
            "Review Bot",
            raw_command,
        ))
        .await;
    let shell_provider = Arc::new(TestShellProvider::new("powershell.exe", &profile_args));
    let profiles = Arc::new(ProfileService::new(
        repository.clone(),
        shell_provider.clone(),
    ));
    let resolver = AgentLaunchResolver::new(Arc::new(PreferencesService::new(repository)))
        .with_profile_service(profiles);

    let resolved = resolver
        .resolve_selected()
        .await
        .expect("known PowerShell profile resolves custom command");

    assert_eq!(resolved.program, "powershell.exe");
    assert_eq!(resolved.args.last().map(String::as_str), Some(raw_command));
    let command_index = resolved
        .args
        .iter()
        .position(|argument| argument == "-Command")
        .expect("custom command has one PowerShell command-mode switch");
    assert_eq!(resolved.args[command_index + 1], raw_command);
    assert_eq!(
        &resolved.args[0..3],
        ["-NoProfile", "-ExecutionPolicy", "Bypass"]
    );
    assert!(resolved.args.contains(&"-NoLogo".to_owned()));
    for no_exit in ["-NoE", "-NOEX", "-NoExI", "-noexit", "/NoE", "--NoEx"] {
        assert!(!resolved.args.contains(&no_exit.to_owned()));
    }
    assert_eq!(
        shell_provider.args,
        profile_args.iter().map(OsString::from).collect::<Vec<_>>(),
        "the ordinary terminal profile remains unchanged"
    );
}

/// cmd 的受管 Agent 命令使用 /c，让 CLI 退出时连同承载它的 shell 一起结束。
///
/// @author kongweiguang
#[cfg(windows)]
#[tokio::test]
async fn windows_custom_cmd_uses_exit_after_command() {
    let repository = Arc::new(MemoryConfigurationRepository::default());
    let raw_command = "review-cli --workspace & echo done";
    repository
        .replace_settings(&selected_custom(
            "11111111-1111-4111-8111-111111111111",
            "Review Bot",
            raw_command,
        ))
        .await;
    let shell_provider = Arc::new(TestShellProvider::new("cmd.exe", &["/d", "/s"]));
    let profiles = Arc::new(ProfileService::new(
        repository.clone(),
        shell_provider.clone(),
    ));
    let resolver = AgentLaunchResolver::new(Arc::new(PreferencesService::new(repository)))
        .with_profile_service(profiles);

    let resolved = resolver
        .resolve_selected()
        .await
        .expect("known cmd profile resolves custom command");

    assert_eq!(resolved.program, "cmd.exe");
    assert_eq!(resolved.args, ["/d", "/s", "/c", raw_command]);
    assert!(!resolved.args.iter().any(|argument| argument == "/k"));
    assert_eq!(
        shell_provider.args,
        [OsString::from("/d"), OsString::from("/s")]
    );
}

/// 不认识的 Windows shell 不猜测通用命令开关或注入未经确认的执行方言。
#[cfg(windows)]
#[tokio::test]
async fn windows_custom_launcher_rejects_unknown_profile_shell() {
    let repository = Arc::new(MemoryConfigurationRepository::default());
    repository
        .replace_settings(&selected_custom(
            "11111111-1111-4111-8111-111111111111",
            "Review Bot",
            "review-cli --safe",
        ))
        .await;
    let profiles = Arc::new(ProfileService::new(
        repository.clone(),
        Arc::new(TestShellProvider::new("fish.exe", &[])),
    ));
    let resolver = AgentLaunchResolver::new(Arc::new(PreferencesService::new(repository)))
        .with_profile_service(profiles);

    assert!(resolver.resolve_selected().await.is_err());
}

/// 每次新建都重新读取偏好，使配置文件外部修改只影响后续会话。
#[cfg(not(windows))]
#[tokio::test]
async fn launch_resolver_observes_latest_settings_without_caching() {
    let repository = Arc::new(MemoryConfigurationRepository::default());
    repository
        .replace_settings(&selected_custom(
            "11111111-1111-4111-8111-111111111111",
            "First",
            "first-cli --one",
        ))
        .await;
    let resolver = AgentLaunchResolver::new(Arc::new(PreferencesService::new(repository.clone())));

    let first = resolver
        .resolve_selected()
        .await
        .expect("first settings resolve");
    assert_eq!(first.program, "first-cli");
    assert_eq!(first.args, ["--one"]);

    repository
        .replace_settings(&selected_custom(
            "22222222-2222-4222-8222-222222222222",
            "Second",
            "second-cli --two",
        ))
        .await;
    let second = resolver
        .resolve_selected()
        .await
        .expect("updated settings resolve");
    assert_eq!(second.program, "second-cli");
    assert_eq!(second.args, ["--two"]);
}

/// Launcher catalog 只列可选身份与显示名，不泄漏自定义命令或声称 CLI 已安装。
///
/// @author kongweiguang
#[tokio::test]
async fn launcher_catalog_contains_safe_configured_options() {
    let repository = Arc::new(MemoryConfigurationRepository::default());
    repository
        .replace_settings(&selected_custom(
            "11111111-1111-4111-8111-111111111111",
            "Private Review Bot",
            "review-cli --token embedded-value",
        ))
        .await;
    let resolver = AgentLaunchResolver::new(Arc::new(PreferencesService::new(repository)));

    let catalog = resolver
        .launcher_catalog(&CancellationToken::new())
        .await
        .expect("validated launcher catalog is readable");

    assert_eq!(catalog.options.len(), 4);
    assert_eq!(
        catalog.selected_key.as_str(),
        "custom:11111111-1111-4111-8111-111111111111"
    );
    assert!(catalog.options.iter().any(|option| {
        option.key == catalog.selected_key
            && option.provider == AgentProvider::Custom
            && option.label == "Private Review Bot"
    }));
    let projection = format!("{catalog:?}");
    assert!(!projection.contains("review-cli"));
    assert!(!projection.contains("embedded-value"));
}

/// 保存 Launcher 选择时只改一个字段，并将已读取的设置 revision 用作 CAS token。
///
/// @author kongweiguang
#[tokio::test]
async fn launcher_selection_cas_preserves_other_settings() {
    let repository = Arc::new(MemoryConfigurationRepository::default());
    let original = selected_custom(
        "11111111-1111-4111-8111-111111111111",
        "Review Bot",
        "review-cli --safe",
    );
    repository.replace_settings(&original).await;
    let resolver = AgentLaunchResolver::new(Arc::new(PreferencesService::new(repository.clone())));
    let key = AgentLauncherKey::builtin(AgentProvider::Claude).expect("Claude key is valid");

    let catalog = resolver
        .select_launcher(key, &CancellationToken::new())
        .await
        .expect("configured built-in choice is persisted");

    let mut expected = original;
    expected.agent_launcher.selected_agent_key = "builtin:claude".to_owned();
    assert_eq!(repository.saved_settings().await, Some(expected));
    assert_eq!(catalog.selected_key.as_str(), "builtin:claude");
}

/// CAS 冲突不覆盖外部编辑，也不把失败的选择报告成已保存。
///
/// @author kongweiguang
#[tokio::test]
async fn launcher_selection_conflict_preserves_saved_settings() {
    let repository = Arc::new(MemoryConfigurationRepository::default());
    let original = selected_custom(
        "11111111-1111-4111-8111-111111111111",
        "Review Bot",
        "review-cli --safe",
    );
    repository.replace_settings(&original).await;
    let mut concurrent = original.clone();
    concurrent.agent_launcher.selected_agent_key = "builtin:claude".to_owned();
    repository.conflict_next_save_with(&concurrent).await;
    let resolver = AgentLaunchResolver::new(Arc::new(PreferencesService::new(repository.clone())));
    let key = AgentLauncherKey::builtin(AgentProvider::Pi).expect("Pi key is valid");

    let result = resolver
        .select_launcher(key, &CancellationToken::new())
        .await;

    assert!(result.is_err());
    assert_eq!(repository.saved_settings().await, Some(concurrent));
}

/// 显式 key 覆盖本次新建选择；若 Custom 已从最新配置删除，则必须失败而不回退。
///
/// @author kongweiguang
#[cfg(not(windows))]
#[tokio::test]
async fn explicit_launcher_is_revalidated_without_fallback() {
    let repository = Arc::new(MemoryConfigurationRepository::default());
    repository
        .replace_settings(&selected_custom(
            "11111111-1111-4111-8111-111111111111",
            "Review Bot",
            "review-cli --safe",
        ))
        .await;
    let resolver = AgentLaunchResolver::new(Arc::new(PreferencesService::new(repository.clone())));
    let claude = AgentLauncherKey::builtin(AgentProvider::Claude).expect("Claude key is valid");
    let resolved = resolver
        .resolve_for_key(Some(&claude), &CancellationToken::new())
        .await
        .expect("explicit configured built-in launcher resolves");
    assert_eq!(resolved.program, "claude");

    repository
        .replace_settings(&SettingsSnapshot::default())
        .await;
    let removed_custom = AgentLauncherKey::new(
        "custom:11111111-1111-4111-8111-111111111111",
        AgentProvider::Custom,
    )
    .expect("custom key has canonical identity");
    let result = resolver
        .resolve_for_key(Some(&removed_custom), &CancellationToken::new())
        .await;
    assert!(result.is_err());
}
