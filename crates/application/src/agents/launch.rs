//! 把当前已校验的 Launcher 偏好解析成一次性 CLI 启动快照。
//!
//! @author kongweiguang

use std::{fmt, sync::Arc};

#[cfg(not(windows))]
use crate::platform::command_line::split_command_line;
use kerminal_domain::agents::{
    AgentLauncherKey, AgentProvider, AgentProviderSession, AgentSessionLaunch,
};
use kerminal_domain::sessions::LocalSessionTarget;

use crate::{
    AppError, AppResult, PreferencesService, ProfileService,
    agents::{AgentLauncherCatalog, AgentLauncherOption},
    preferences::{CustomAgentDefinition, SettingsSnapshot},
};
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
type ProfileShellTarget = LocalSessionTarget;
#[cfg(not(windows))]
type ProfileShellTarget = ();

const PI_AGENT_LAUNCH_ARGS: [&str; 1] = ["--approve"];
const PI_AGENT_RESUME_COMMAND: &str = "pi --approve --continue";
const MAX_SAVED_RESUME_COMMAND_BYTES: usize = 8 * 1024;
const MAX_PROVIDER_SESSION_ID_BYTES: usize = 1024;

/// 已解析的启动命令；只在创建新会话时读取，不作为运行中的配置源。
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedAgentLauncher {
    pub provider: AgentProvider,
    pub launcher_key: AgentLauncherKey,
    pub command_label: String,
    pub program: String,
    pub args: Vec<String>,
}

impl fmt::Debug for ResolvedAgentLauncher {
    /// CLI 参数可能包含用户放入命令行的秘密，调试输出只呈现参数数量。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedAgentLauncher")
            .field("provider", &self.provider)
            .field("launcher_key", &self.launcher_key)
            .field("command_label", &self.command_label)
            .field("argument_count", &self.args.len())
            .field("command_values", &"<redacted>")
            .finish()
    }
}

impl ResolvedAgentLauncher {
    /// 将配置选择固化为域对象快照；cwd 由应用创建出的隔离 workspace 决定。
    pub fn into_session_launch(self, cwd: String) -> AppResult<AgentSessionLaunch> {
        let launch = AgentSessionLaunch {
            command_label: self.command_label,
            shell: self.program,
            args: self.args,
            cwd,
        };
        launch
            .validate()
            .map_err(|error| AppError::invalid("Agent 启动定义无效").with_source(error))?;
        Ok(launch)
    }
}

/// 每次新建都即时读取配置；该解析器不缓存偏好，也不触碰已有会话快照。
pub struct AgentLaunchResolver {
    preferences: Arc<PreferencesService>,
    profile_service: Option<Arc<ProfileService>>,
}

impl AgentLaunchResolver {
    /// 复用桌面组合根的 PreferencesService，确保 UI 与 Agent 新建使用同一配置来源。
    pub fn new(preferences: Arc<PreferencesService>) -> Self {
        Self {
            preferences,
            profile_service: None,
        }
    }

    /// 复用主工作台的 profile 选择，Windows 自定义命令只交给已识别的真实 shell 执行。
    pub fn with_profile_service(mut self, profile_service: Arc<ProfileService>) -> Self {
        self.profile_service = Some(profile_service);
        self
    }

    /// 从每次读取的已验证设置生成无命令内容的 launcher 目录，不探测本机是否安装 CLI。
    ///
    /// @author kongweiguang
    pub async fn launcher_catalog(
        &self,
        cancellation: &CancellationToken,
    ) -> AppResult<AgentLauncherCatalog> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("Agent launcher catalog was cancelled"));
        }
        let settings = self.preferences.load_settings().await?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("Agent launcher catalog was cancelled"));
        }
        launcher_catalog_from_settings(&settings.settings)
    }

    /// 仅修改 selected key，并把读取时的 revision 原样用于 CAS，避免覆盖并发设置编辑。
    ///
    /// @author kongweiguang
    pub async fn select_launcher(
        &self,
        key: AgentLauncherKey,
        cancellation: &CancellationToken,
    ) -> AppResult<AgentLauncherCatalog> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled(
                "Agent launcher selection was cancelled",
            ));
        }
        let current = self.preferences.load_settings().await?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled(
                "Agent launcher selection was cancelled",
            ));
        }
        let current_catalog = launcher_catalog_from_settings(&current.settings)?;
        if !current_catalog
            .options
            .iter()
            .any(|option| option.key == key)
        {
            return Err(AppError::not_found(
                "Agent launcher is no longer configured",
            ));
        }
        if current_catalog.selected_key == key {
            return Ok(current_catalog);
        }

        let mut settings = current.settings;
        settings.agent_launcher.selected_agent_key = key.as_str().to_owned();
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled(
                "Agent launcher selection was cancelled",
            ));
        }
        let saved = self
            .preferences
            .save_settings(settings, current.revision)
            .await?;
        launcher_catalog_from_settings(&saved.settings)
    }

    /// 读取并验证当前选择，外部修改会影响后续新建，但不会重启活动 CLI。
    ///
    /// @author kongweiguang
    pub async fn resolve_selected(&self) -> AppResult<ResolvedAgentLauncher> {
        self.resolve_for_key(None, &CancellationToken::new()).await
    }

    /// 按本次显式 key 或最新持久选择解析启动命令；显式 key 缺失时绝不回退默认项。
    ///
    /// @author kongweiguang
    pub async fn resolve_for_key(
        &self,
        requested_key: Option<&AgentLauncherKey>,
        cancellation: &CancellationToken,
    ) -> AppResult<ResolvedAgentLauncher> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled(
                "Agent launcher resolution was cancelled",
            ));
        }
        let versioned = self.preferences.load_settings().await?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled(
                "Agent launcher resolution was cancelled",
            ));
        }
        let catalog = launcher_catalog_from_settings(&versioned.settings)?;
        let key = requested_key
            .cloned()
            .unwrap_or_else(|| catalog.selected_key.clone());
        if !catalog.options.iter().any(|option| option.key == key) {
            return Err(AppError::not_found(
                "Requested Agent launcher is no longer configured",
            ));
        }
        let mut settings = versioned.settings;
        settings.agent_launcher.selected_agent_key = key.as_str().to_owned();
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled(
                "Agent launcher resolution was cancelled",
            ));
        }
        #[cfg(windows)]
        if settings
            .agent_launcher
            .selected_agent_key
            .starts_with("custom:")
        {
            let profiles = self.profile_service.as_ref().ok_or_else(|| {
                AppError::unsupported("Windows 自定义 Agent 需要已装配的本地 profile 服务")
            })?;
            let shell = profiles.default_local_session_target().await?;
            if cancellation.is_cancelled() {
                return Err(AppError::cancelled(
                    "Agent launcher resolution was cancelled",
                ));
            }
            return resolve_agent_launcher_with_shell(&settings, &shell);
        }
        resolve_agent_launcher(&settings)
    }

    /// 按持久 provider 元数据和当前冻结策略恢复，并与列表投影共用纯恢复定义校验。
    /// 运行时不从 provider 类型推导缺失命令，也不把 profile 或 CLI 可用性写成元数据能力。
    ///
    /// @author kongweiguang
    pub(super) async fn resolve_resume_launch(
        &self,
        metadata: &AgentProviderSession,
        cwd: String,
    ) -> AppResult<AgentSessionLaunch> {
        let command = validated_resume_command(metadata)?;
        let (command_label, shell, args) =
            if is_frozen_default_resume_command(metadata.provider, command) {
                let (program, args) = builtin_resume_argv(
                    metadata.provider,
                    metadata.provider_session_id.as_deref(),
                )?;
                (resume_label(metadata.provider), program, args)
            } else {
                let (program, args) = self.resolve_profile_shell_command(command).await?;
                (resume_label(metadata.provider), program, args)
            };

        let launch = AgentSessionLaunch {
            command_label,
            shell,
            args,
            cwd,
        };
        launch
            .validate()
            .map_err(|error| AppError::invalid("Agent 恢复启动定义无效").with_source(error))?;
        Ok(launch)
    }

    /// 将 legacy 单行命令交给确切识别的默认 shell；不猜引号规则或自行切分 argv。
    async fn resolve_profile_shell_command(
        &self,
        command: &str,
    ) -> AppResult<(String, Vec<String>)> {
        let profiles = self.profile_service.as_ref().ok_or_else(|| {
            AppError::unsupported("恢复旧 Agent 命令需要已装配的本地 shell profile")
        })?;
        let target = profiles.default_local_session_target().await?;
        shell_command_target(&target, command)
    }
}

/// 将已验证设置投影为允许展示的启动器选项；自定义命令和其它定义字段不离开应用层。
///
/// @author kongweiguang
fn launcher_catalog_from_settings(settings: &SettingsSnapshot) -> AppResult<AgentLauncherCatalog> {
    let settings = settings.clone().validated()?;
    let builtins = [
        (AgentProvider::Codex, "Codex"),
        (AgentProvider::Claude, "Claude Code"),
        (AgentProvider::Pi, "Pi"),
    ];
    let mut options = builtins
        .into_iter()
        .map(|(provider, label)| {
            let key = AgentLauncherKey::builtin(provider).map_err(|error| {
                AppError::invalid("Built-in Agent launcher is invalid").with_source(error)
            })?;
            Ok(AgentLauncherOption {
                key,
                provider,
                label: label.to_owned(),
            })
        })
        .collect::<AppResult<Vec<_>>>()?;

    for definition in &settings.agent_launcher.custom_agents {
        let key = AgentLauncherKey::new(format!("custom:{}", definition.id), AgentProvider::Custom)
            .map_err(|error| {
                AppError::invalid("Custom Agent launcher is invalid").with_source(error)
            })?;
        options.push(AgentLauncherOption {
            key,
            provider: AgentProvider::Custom,
            label: definition.name.clone(),
        });
    }

    let selected_key = options
        .iter()
        .find(|option| option.key.as_str() == settings.agent_launcher.selected_agent_key)
        .map(|option| option.key.clone())
        .ok_or_else(|| AppError::configuration_corrupt("selected Agent launcher"))?;
    Ok(AgentLauncherCatalog {
        selected_key,
        options,
    })
}

/// 构造当前 provider 初始元数据；调用方先通过 AgentSession 领域校验再传入 provider。
///
/// 本函数只创建持久化值，不写文件或启动 CLI；Custom 不推测恢复命令和 provider 会话 ID。
///
/// @author kongweiguang
pub fn initial_provider_session(provider: AgentProvider) -> AgentProviderSession {
    let (resume_supported, resume_command) = match provider {
        AgentProvider::Codex => (true, Some("codex resume --last".to_owned())),
        AgentProvider::Claude => (true, Some("claude --continue".to_owned())),
        AgentProvider::Pi => (true, Some(PI_AGENT_RESUME_COMMAND.to_owned())),
        AgentProvider::Custom => (false, None),
    };
    AgentProviderSession {
        schema_version: 1,
        provider,
        provider_session_id: None,
        resume_command,
        resume_supported,
        last_resume_at: None,
    }
}

/// 复用恢复入口的纯元数据校验，让列表投影不读取当前 shell profile 或探测外部 CLI。
///
/// @author kongweiguang
pub(super) fn validated_resume_command(metadata: &AgentProviderSession) -> AppResult<&str> {
    metadata.validate().map_err(|error| {
        AppError::configuration_corrupt("Agent provider metadata").with_source(error)
    })?;
    let command = require_saved_resume_command(metadata)?;
    if is_frozen_default_resume_command(metadata.provider, command) {
        builtin_resume_argv(metadata.provider, metadata.provider_session_id.as_deref())?;
    }
    Ok(command)
}

/// 根据当前能力标记要求持久化恢复命令；缺失 builtin 元数据不能在运行时被默认策略掩盖。
///
/// @author kongweiguang
fn require_saved_resume_command(metadata: &AgentProviderSession) -> AppResult<&str> {
    if !metadata.resume_supported {
        return Err(AppError::unsupported(
            "此 Agent provider 未声明会话恢复能力",
        ));
    }
    let Some(command) = metadata.resume_command.as_deref() else {
        return Err(if metadata.provider == AgentProvider::Custom {
            AppError::unsupported("自定义 Agent 缺少已保存的恢复命令")
        } else {
            AppError::configuration_corrupt("Agent provider metadata")
        });
    };
    validate_saved_resume_command(command)
}

/// 校验持久单行命令，避免换行、NUL 或超长内容进入 shell 启动参数。
fn validate_saved_resume_command(command: &str) -> AppResult<&str> {
    let command = command.trim();
    if command.is_empty()
        || command.len() > MAX_SAVED_RESUME_COMMAND_BYTES
        || command.chars().any(char::is_control)
    {
        return Err(AppError::invalid("Agent provider 恢复命令无效"));
    }
    Ok(command)
}

/// 只接受有界、无控制字符的 provider ID，并把它作为一个独立 argv 传给 CLI。
fn validate_provider_session_id(session_id: &str) -> AppResult<&str> {
    let session_id = session_id.trim();
    if session_id.is_empty()
        || session_id.len() > MAX_PROVIDER_SESSION_ID_BYTES
        || session_id.chars().any(char::is_control)
        || session_id.starts_with('-')
    {
        return Err(AppError::invalid("Agent provider 会话 ID 无效"));
    }
    Ok(session_id)
}

/// 校验当前内建 provider 的会话标识并生成冻结 argv，供运行时与一次性迁移共用策略。
///
/// @author kongweiguang
pub fn builtin_resume_argv(
    provider: AgentProvider,
    provider_session_id: Option<&str>,
) -> AppResult<(String, Vec<String>)> {
    let provider_session_id = provider_session_id
        .map(validate_provider_session_id)
        .transpose()?;
    let (program, args) = match (provider, provider_session_id) {
        (AgentProvider::Codex, Some(id)) => ("codex", vec!["resume".to_owned(), id.to_owned()]),
        (AgentProvider::Codex, None) => ("codex", vec!["resume".to_owned(), "--last".to_owned()]),
        (AgentProvider::Claude, Some(id)) => ("claude", vec!["--resume".to_owned(), id.to_owned()]),
        (AgentProvider::Claude, None) => ("claude", vec!["--continue".to_owned()]),
        (AgentProvider::Pi, None) => (
            "pi",
            PI_AGENT_LAUNCH_ARGS
                .iter()
                .map(|argument| (*argument).to_owned())
                .chain(std::iter::once("--continue".to_owned()))
                .collect(),
        ),
        (AgentProvider::Pi, Some(_)) => {
            return Err(AppError::unsupported(
                "Pi provider 会话 ID 没有冻结的恢复参数映射",
            ));
        }
        (AgentProvider::Custom, _) => {
            return Err(AppError::unsupported(
                "自定义 Agent 只能使用显式保存的恢复命令",
            ));
        }
    };
    Ok((program.to_owned(), args))
}

/// 只有与冻结默认命令完全相同的文本才走结构化内建参数映射。
fn is_frozen_default_resume_command(provider: AgentProvider, command: &str) -> bool {
    matches!(
        (provider, command),
        (AgentProvider::Codex, "codex resume --last")
            | (AgentProvider::Claude, "claude --continue")
            | (AgentProvider::Pi, PI_AGENT_RESUME_COMMAND)
    )
}

/// 为 UI 提供稳定 provider 标签，不把可能含用户数据的恢复命令复制到 label。
fn resume_label(provider: AgentProvider) -> String {
    format!("{} resume", provider.as_str())
}

#[cfg(test)]
#[path = "../../tests/unit/agent_resume.rs"]
mod tests;

/// 只按 shell 可执行名识别命令语义，再保留 profile 参数并追加原始命令；Windows Agent 恢复同样移除持久 NoExit。
///
/// @author kongweiguang
fn shell_command_target(
    target: &LocalSessionTarget,
    command: &str,
) -> AppResult<(String, Vec<String>)> {
    let program = target
        .program
        .to_str()
        .ok_or_else(|| AppError::unsupported("本地 profile shell 路径不是有效文本"))?
        .to_owned();
    let basename = program
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(&program)
        .to_ascii_lowercase();
    let mut args = target
        .args
        .iter()
        .map(|argument| {
            argument
                .to_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| AppError::unsupported("本地 profile shell 参数不是有效文本"))
        })
        .collect::<AppResult<Vec<_>>>()?;

    match basename.as_str() {
        "bash" | "zsh" => {
            reject_existing_command_mode(&args, &["-c", "--command"])?;
            args.push("-lc".to_owned());
            args.push(command.to_owned());
        }
        "sh" | "dash" | "ksh" | "mksh" => {
            reject_existing_command_mode(&args, &["-c", "--command"])?;
            args.push("-c".to_owned());
            args.push(command.to_owned());
        }
        "fish" => {
            reject_existing_command_mode(&args, &["-c", "--command"])?;
            args.extend([
                "--login".to_owned(),
                "--command".to_owned(),
                command.to_owned(),
            ]);
        }
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => {
            reject_existing_command_mode(
                &args,
                &["-command", "-c", "-file", "-encodedcommand", "-e"],
            )?;
            #[cfg(windows)]
            remove_powershell_noexit_switches(&mut args);
            args.extend([
                "-NoLogo".to_owned(),
                "-Command".to_owned(),
                command.to_owned(),
            ]);
        }
        "cmd" | "cmd.exe" => {
            reject_existing_command_mode(&args, &["/c", "/k"])?;
            args.extend([
                "/d".to_owned(),
                "/s".to_owned(),
                "/c".to_owned(),
                command.to_owned(),
            ]);
        }
        _ => {
            return Err(AppError::unsupported(
                "本地 profile shell 方言未识别，无法恢复旧命令",
            ));
        }
    }
    Ok((program, args))
}

/// 避免在用户 profile 已进入命令模式后再次追加命令造成解释器语义歧义。
fn reject_existing_command_mode(args: &[String], flags: &[&str]) -> AppResult<()> {
    if args
        .iter()
        .any(|argument| flags.iter().any(|flag| argument.eq_ignore_ascii_case(flag)))
    {
        return Err(AppError::unsupported("本地 profile shell 已指定命令模式"));
    }
    Ok(())
}

/// 将已校验设置中的 selected key 映射到内置 CLI 或指定的 Custom argv。
pub fn resolve_agent_launcher(settings: &SettingsSnapshot) -> AppResult<ResolvedAgentLauncher> {
    resolve_agent_launcher_inner(settings, None)
}

/// 让 Windows 自定义命令运行在主工作台实际选中的已知 shell，持久化结果仍是 shell+args。
#[cfg(windows)]
fn resolve_agent_launcher_with_shell(
    settings: &SettingsSnapshot,
    shell: &LocalSessionTarget,
) -> AppResult<ResolvedAgentLauncher> {
    resolve_agent_launcher_inner(settings, Some(shell))
}

/// 只在 Windows custom 分支需要注入 shell；其他 provider 保持确定的可执行程序与参数。
fn resolve_agent_launcher_inner(
    settings: &SettingsSnapshot,
    shell: Option<&ProfileShellTarget>,
) -> AppResult<ResolvedAgentLauncher> {
    #[cfg(not(windows))]
    let _ = shell;
    let settings = settings.clone().validated()?;
    let selected = settings.agent_launcher.selected_agent_key.as_str();
    let (provider, key, command_label, program, args) = match selected {
        "builtin:codex" => (
            AgentProvider::Codex,
            selected.to_owned(),
            "Codex".to_owned(),
            "codex".to_owned(),
            vec![
                "--dangerously-bypass-approvals-and-sandbox".to_owned(),
                "--dangerously-bypass-hook-trust".to_owned(),
            ],
        ),
        "builtin:claude" => (
            AgentProvider::Claude,
            selected.to_owned(),
            "Claude Code".to_owned(),
            "claude".to_owned(),
            vec!["--dangerously-skip-permissions".to_owned()],
        ),
        "builtin:pi" => (
            AgentProvider::Pi,
            selected.to_owned(),
            "Pi".to_owned(),
            "pi".to_owned(),
            PI_AGENT_LAUNCH_ARGS
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
        ),
        custom_key => {
            #[cfg(windows)]
            {
                let shell = shell.ok_or_else(|| {
                    AppError::unsupported("Windows 自定义 Agent 需要按实际本地 shell 解析启动命令")
                })?;
                resolve_custom_launcher(custom_key, &settings.agent_launcher.custom_agents, shell)?
            }
            #[cfg(not(windows))]
            {
                resolve_custom_launcher(custom_key, &settings.agent_launcher.custom_agents)?
            }
        }
    };
    let launcher_key = AgentLauncherKey::new(key, provider)
        .map_err(|error| AppError::invalid("Agent launcher key 无效").with_source(error))?;
    Ok(ResolvedAgentLauncher {
        provider,
        launcher_key,
        command_label,
        program,
        args,
    })
}

/// Agent CLI 必须结束时让 PTY 一并退出，因此仅在派生参数中移除 PowerShell 的 NoExit 开关；普通终端 Profile 原样保留。
/// PowerShell 官方解析器按不区分大小写的最短前缀 `noe` 匹配 NoExit，范围是 3 至 6 个字符。
/// 参考：https://github.com/PowerShell/PowerShell/blob/master/src/Microsoft.PowerShell.ConsoleHost/host/msh/CommandLineParameterParser.cs
///
/// @author kongweiguang
#[cfg(windows)]
fn resolve_custom_launcher(
    selected_key: &str,
    definitions: &[CustomAgentDefinition],
    shell: &LocalSessionTarget,
) -> AppResult<(AgentProvider, String, String, String, Vec<String>)> {
    let id = selected_key
        .strip_prefix("custom:")
        .ok_or_else(|| AppError::invalid("Agent 选择项无效"))?;
    let definition = definitions
        .iter()
        .find(|definition| definition.id == id)
        .ok_or_else(|| AppError::invalid("所选自定义 Agent 定义不存在"))?;
    if definition.command.contains('\0') {
        return Err(AppError::invalid("自定义 Agent 命令包含无效字符"));
    }
    let program = shell
        .program
        .to_str()
        .ok_or_else(|| AppError::invalid("本地 profile shell 路径必须是有效文本"))?
        .to_owned();
    let basename = program
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(&program)
        .to_ascii_lowercase();
    let mut args = shell
        .args
        .iter()
        .map(|argument| {
            argument
                .to_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| AppError::invalid("本地 profile shell 参数必须是有效文本"))
        })
        .collect::<AppResult<Vec<_>>>()?;
    match basename.as_str() {
        "powershell.exe" | "powershell" | "pwsh.exe" | "pwsh" => {
            if args.iter().any(|argument| {
                matches!(
                    argument.to_ascii_lowercase().as_str(),
                    "-command" | "-c" | "-file" | "-encodedcommand" | "-e"
                )
            }) {
                return Err(AppError::invalid(
                    "本地 PowerShell profile 已指定命令模式，无法安全追加 Agent 命令",
                ));
            }
            remove_powershell_noexit_switches(&mut args);
            push_flag_if_missing(&mut args, "-NoLogo");
            push_flag_if_missing(&mut args, "-NoProfile");
            args.push("-Command".to_owned());
            args.push(definition.command.clone());
        }
        "cmd.exe" | "cmd" => {
            if args
                .iter()
                .any(|argument| matches!(argument.to_ascii_lowercase().as_str(), "/c" | "/k"))
            {
                return Err(AppError::invalid(
                    "本地 cmd profile 已指定命令模式，无法安全追加 Agent 命令",
                ));
            }
            push_flag_if_missing(&mut args, "/d");
            push_flag_if_missing(&mut args, "/s");
            args.push("/c".to_owned());
            args.push(definition.command.clone());
        }
        _ => {
            return Err(AppError::unsupported(
                "当前本地 profile shell 不支持受管 Agent shell 命令",
            ));
        }
    }
    Ok((
        AgentProvider::Custom,
        selected_key.to_owned(),
        definition.name.clone(),
        program,
        args,
    ))
}

/// PowerShell CLI 结束后应由现有 PTY 生命周期观察到退出，故只清理 Profile 参数副本中可匹配的 NoExit 开关。
/// PowerShell parser 接受 `-`、`--` 和 Windows `/` 前缀；不区分大小写并允许 `noe` 至 `noexit` 的有限缩写。
///
/// @author kongweiguang
#[cfg(windows)]
fn remove_powershell_noexit_switches(args: &mut Vec<String>) {
    args.retain(|argument| !is_powershell_noexit_switch(argument));
}

/// 依据 PowerShell 的最短前缀规则识别 NoExit，而不误删其它 Profile 参数。
///
/// @author kongweiguang
#[cfg(windows)]
fn is_powershell_noexit_switch(argument: &str) -> bool {
    let key = argument
        .strip_prefix("--")
        .or_else(|| argument.strip_prefix('-'))
        .or_else(|| argument.strip_prefix('/'));
    let Some(key) = key else {
        return false;
    };
    let key = key.to_ascii_lowercase();
    (3..=6).contains(&key.len()) && "noexit".starts_with(&key)
}

/// 保留 profile 已配置的开关，并避免重复加入 shell 初始化参数。
#[cfg(windows)]
fn push_flag_if_missing(args: &mut Vec<String>, flag: &str) {
    if !args
        .iter()
        .any(|argument| argument.eq_ignore_ascii_case(flag))
    {
        args.push(flag.to_owned());
    }
}

/// Unix 自定义命令沿用旧实现的无 shell 分词，不把管道或变量展开猜成可执行行为。
#[cfg(not(windows))]
fn resolve_custom_launcher(
    selected_key: &str,
    definitions: &[CustomAgentDefinition],
) -> AppResult<(AgentProvider, String, String, String, Vec<String>)> {
    let id = selected_key
        .strip_prefix("custom:")
        .ok_or_else(|| AppError::invalid("Agent 选择项无效"))?;
    let definition = definitions
        .iter()
        .find(|definition| definition.id == id)
        .ok_or_else(|| AppError::invalid("所选自定义 Agent 定义不存在"))?;
    if definition.command.contains('\0') {
        return Err(AppError::invalid("自定义 Agent 命令包含无效字符"));
    }
    let mut args = split_command_line(&definition.command)
        .map_err(|_| AppError::invalid("自定义 Agent 命令引号格式无效"))?;
    if args.is_empty() || args[0].trim().is_empty() {
        return Err(AppError::invalid("自定义 Agent 命令缺少可执行程序"));
    }
    let program = args.remove(0);
    Ok((
        AgentProvider::Custom,
        selected_key.to_owned(),
        definition.name.clone(),
        program,
        args,
    ))
}
