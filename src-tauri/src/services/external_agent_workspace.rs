//! External agent workspace file preparation.
//!
//! @author kongweiguang

use std::{collections::BTreeMap, fs, io::ErrorKind, path::PathBuf};

use crate::{
    error::{AppError, AppResult},
    models::agent_session::{
        AgentId, AgentProvider, AgentProviderSession, AgentSessionId, PI_AGENT_LAUNCH_COMMAND,
        PI_AGENT_RESUME_COMMAND,
    },
    services::agent_session_file_store::AgentSessionFileStore,
};

const DEFAULT_MCP_ENDPOINT: &str = "http://127.0.0.1:37657/mcp";
const MANAGED_BLOCK_START: &str = "<!-- KERMINAL_EXTERNAL_AGENT_START -->";
const MANAGED_BLOCK_END: &str = "<!-- KERMINAL_EXTERNAL_AGENT_END -->";
const CONFIG_REFERENCE_FILE_NAME: &str = "kerminal-config.md";
const CONFIG_VALIDATOR_TOOL_ID: &str = "kerminal.config.validate";
const AGENT_SESSION_TERMINAL_SNAPSHOT_BYTES: usize = 24 * 1024;
#[cfg(windows)]
const WINDOWS_AGENT_PWSH: &str = "pwsh.exe";
#[cfg(windows)]
const WINDOWS_AGENT_POWERSHELL: &str = "powershell.exe";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;
mod helpers;
mod templates;
mod types;
mod workspace_files;

use self::helpers::*;
use self::types::{AgentSessionWorkspaceContext, WorkspaceTextPlan, WorkspaceWriteOptions};
pub use helpers::rules;
pub(crate) use templates::CONFIG_REFERENCE_BODY;
pub use types::{
    ExternalAgentFileAction, ExternalAgentFileOperation, ExternalAgentLaunchSpec,
    ExternalAgentOverwritePolicy, ExternalAgentStatus, ExternalAgentStatuses,
    ExternalAgentValidatorStatus, ExternalAgentWorkspaceStatus,
    PrepareExternalAgentWorkspaceRequest,
};

#[derive(Debug, Clone)]
pub struct ExternalAgentWorkspaceService {
    workspace_dir: PathBuf,
    mcp_endpoint: String,
    mcp_server_running: bool,
}

impl ExternalAgentWorkspaceService {
    /// 创建 workspace service；CLI 状态延迟到 status() 探测，避免初始化产生子进程副作用。
    pub fn new(
        workspace_dir: impl Into<PathBuf>,
        mcp_endpoint: Option<String>,
        mcp_server_running: bool,
    ) -> Self {
        Self {
            workspace_dir: workspace_dir.into(),
            mcp_endpoint: mcp_endpoint.unwrap_or_else(|| DEFAULT_MCP_ENDPOINT.to_owned()),
            mcp_server_running,
        }
    }

    /// 返回四类 provider 的可用性快照；Pi 保留兼容 adapter 字段但不探测旧 adapter。
    pub fn status(&self) -> ExternalAgentWorkspaceStatus {
        ExternalAgentWorkspaceStatus {
            workspace_dir: path_to_string(&self.workspace_dir),
            mcp_endpoint: self.mcp_endpoint.clone(),
            mcp_server_running: self.mcp_server_running,
            agents: ExternalAgentStatuses {
                codex: self.agent_status("codex", "Codex", "codex", self.codex_config_path()),
                claude: self.agent_status("claude", "Claude", "claude", self.claude_config_path()),
                pi: self.pi_agent_status(),
                custom: self.custom_agent_status(),
            },
            validator: self.validator_status(),
        }
    }

    /// 初始化内置 provider 共用规则及各自原生 MCP 配置，不触碰自定义命令定义。
    pub fn ensure_default_agent_files(&self) -> AppResult<()> {
        fs::create_dir_all(&self.workspace_dir)?;
        let options = WorkspaceWriteOptions::write_default();
        self.prepare_codex_files(&options)?;
        self.prepare_claude_files(&options)?;
        self.prepare_pi_files(&options)?;
        Ok(())
    }

    /// 准备无 session 或 session-scoped 启动；只生成文件和 launch spec，不直接拉起 CLI。
    pub fn prepare(
        &self,
        request: &PrepareExternalAgentWorkspaceRequest,
    ) -> AppResult<ExternalAgentLaunchSpec> {
        if let Some(agent_session_id) = request
            .agent_session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return self.prepare_agent_session_workspace(request, agent_session_id);
        }

        let options = WorkspaceWriteOptions::from_request(request);
        if !options.dry_run {
            fs::create_dir_all(&self.workspace_dir)?;
        }
        let agent_id = request.agent_id.trim();

        match agent_id {
            "codex" => {
                let operations = self.prepare_codex_files(&options)?;
                let (shell, args) = agent_launch_command("codex");
                Ok(ExternalAgentLaunchSpec {
                    agent_id: "codex".to_owned(),
                    agent_session_id: None,
                    title: "Codex".to_owned(),
                    shell,
                    args,
                    cwd: path_to_string(&self.workspace_dir),
                    env: None,
                    message: if options.dry_run {
                        "Codex workspace file changes were previewed.".to_owned()
                    } else {
                        "Codex workspace files are ready.".to_owned()
                    },
                    dry_run: options.dry_run,
                    operations,
                    validator: self.validator_status(),
                })
            }
            "claude" => {
                let operations = self.prepare_claude_files(&options)?;
                let (shell, args) = agent_launch_command("claude");
                Ok(ExternalAgentLaunchSpec {
                    agent_id: "claude".to_owned(),
                    agent_session_id: None,
                    title: "Claude".to_owned(),
                    shell,
                    args,
                    cwd: path_to_string(&self.workspace_dir),
                    env: None,
                    message: if options.dry_run {
                        "Claude workspace file changes were previewed.".to_owned()
                    } else {
                        "Claude workspace files are ready.".to_owned()
                    },
                    dry_run: options.dry_run,
                    operations,
                    validator: self.validator_status(),
                })
            }
            "pi" => {
                let operations = self.prepare_pi_files(&options)?;
                let (shell, args) = agent_launch_command(PI_AGENT_LAUNCH_COMMAND);
                Ok(ExternalAgentLaunchSpec {
                    agent_id: "pi".to_owned(),
                    agent_session_id: None,
                    title: "PI Agent".to_owned(),
                    shell,
                    args,
                    cwd: path_to_string(&self.workspace_dir),
                    env: None,
                    message: if options.dry_run {
                        "PI Agent workspace file changes were previewed.".to_owned()
                    } else {
                        "PI Agent workspace files are ready.".to_owned()
                    },
                    dry_run: options.dry_run,
                    operations,
                    validator: self.validator_status(),
                })
            }
            "custom" => {
                let command = request
                    .custom_command
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| {
                        AppError::InvalidInput(
                            "Custom agent command is not configured. Enter a command before launch."
                                .to_owned(),
                        )
                    })?;
                let (shell, args) = agent_launch_command(&command);
                Ok(ExternalAgentLaunchSpec {
                    agent_id: "custom".to_owned(),
                    agent_session_id: None,
                    title: "Custom Agent".to_owned(),
                    shell,
                    args,
                    cwd: path_to_string(&self.workspace_dir),
                    env: None,
                    message: "Custom agent workspace is ready.".to_owned(),
                    dry_run: options.dry_run,
                    operations: Vec::new(),
                    validator: self.validator_status(),
                })
            }
            other => Err(AppError::InvalidInput(format!(
                "Unsupported external agent: {other}"
            ))),
        }
    }

    /// 为右栏 Agent session 准备隔离 cwd 和上下文；Pi 的原生配置必须与启动 cwd 同目录。
    pub fn prepare_agent_session_workspace(
        &self,
        request: &PrepareExternalAgentWorkspaceRequest,
        agent_session_id: &str,
    ) -> AppResult<ExternalAgentLaunchSpec> {
        let options = WorkspaceWriteOptions::from_request(request);
        let agent_id = request.agent_id.trim();
        let context = self.agent_session_context(agent_id, agent_session_id)?;

        if !options.dry_run {
            fs::create_dir_all(&self.workspace_dir)?;
            fs::create_dir_all(&context.session_root)?;
        }

        match agent_id {
            "codex" => {
                let mut operations = self.prepare_codex_files(&options)?;
                operations
                    .extend(self.prepare_agent_session_common_files(&context, true, &options)?);
                operations.extend(self.prepare_agent_session_provider_files(&context, &options)?);
                let (_command_label, shell, args) = self.agent_session_launch_command(
                    AgentId::Codex,
                    "codex",
                    &context,
                    request.resume_provider_session,
                    &options,
                )?;
                Ok(ExternalAgentLaunchSpec {
                    agent_id: "codex".to_owned(),
                    agent_session_id: Some(context.agent_session_id.clone()),
                    title: "Codex".to_owned(),
                    shell,
                    args,
                    cwd: path_to_string(&context.session_root),
                    env: Some(self.agent_session_env(&context)),
                    message: if options.dry_run {
                        "Codex agent session workspace file changes were previewed.".to_owned()
                    } else {
                        "Codex agent session workspace files are ready.".to_owned()
                    },
                    dry_run: options.dry_run,
                    operations,
                    validator: self.validator_status(),
                })
            }
            "claude" => {
                let mut operations = self.prepare_claude_files(&options)?;
                operations
                    .extend(self.prepare_agent_session_common_files(&context, true, &options)?);
                operations.extend(self.prepare_agent_session_provider_files(&context, &options)?);
                let (_command_label, shell, args) = self.agent_session_launch_command(
                    AgentId::Claude,
                    "claude",
                    &context,
                    request.resume_provider_session,
                    &options,
                )?;
                Ok(ExternalAgentLaunchSpec {
                    agent_id: "claude".to_owned(),
                    agent_session_id: Some(context.agent_session_id.clone()),
                    title: "Claude".to_owned(),
                    shell,
                    args,
                    cwd: path_to_string(&context.session_root),
                    env: Some(self.agent_session_env(&context)),
                    message: if options.dry_run {
                        "Claude agent session workspace file changes were previewed.".to_owned()
                    } else {
                        "Claude agent session workspace files are ready.".to_owned()
                    },
                    dry_run: options.dry_run,
                    operations,
                    validator: self.validator_status(),
                })
            }
            "pi" => {
                let mut operations = self.prepare_pi_files(&options)?;
                operations
                    .extend(self.prepare_agent_session_common_files(&context, false, &options)?);
                operations.push(self.ensure_agent_session_pi_mcp_json(&context, &options)?);
                let (_command_label, shell, args) = self.agent_session_launch_command(
                    AgentId::Pi,
                    PI_AGENT_LAUNCH_COMMAND,
                    &context,
                    request.resume_provider_session,
                    &options,
                )?;
                Ok(ExternalAgentLaunchSpec {
                    agent_id: "pi".to_owned(),
                    agent_session_id: Some(context.agent_session_id.clone()),
                    title: "PI Agent".to_owned(),
                    shell,
                    args,
                    cwd: path_to_string(&context.session_root),
                    env: Some(self.agent_session_env(&context)),
                    message: if options.dry_run {
                        "PI Agent session workspace file changes were previewed.".to_owned()
                    } else {
                        "PI Agent session workspace files are ready.".to_owned()
                    },
                    dry_run: options.dry_run,
                    operations,
                    validator: self.validator_status(),
                })
            }
            "custom" => {
                let command = request
                    .custom_command
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| {
                        AppError::InvalidInput(
                            "Custom agent command is not configured. Enter a command before launch."
                                .to_owned(),
                        )
                    })?;
                let operations =
                    self.prepare_agent_session_common_files(&context, false, &options)?;
                let (shell, args) = agent_launch_command(&command);
                self.sync_agent_session_launch(&context, &command, &shell, &args, &options)?;
                let title = self.custom_agent_session_title(&context)?;
                Ok(ExternalAgentLaunchSpec {
                    agent_id: "custom".to_owned(),
                    agent_session_id: Some(context.agent_session_id.clone()),
                    title,
                    shell,
                    args,
                    cwd: path_to_string(&context.session_root),
                    env: Some(self.agent_session_env(&context)),
                    message: if options.dry_run {
                        "Custom agent session workspace file changes were previewed.".to_owned()
                    } else {
                        "Custom agent session workspace files are ready.".to_owned()
                    },
                    dry_run: options.dry_run,
                    operations,
                    validator: self.validator_status(),
                })
            }
            other => Err(AppError::InvalidInput(format!(
                "Unsupported external agent: {other}"
            ))),
        }
    }

    /// 选择新建/恢复命令并把实际命令快照回写 session.toml，保证后续历史恢复一致。
    fn agent_session_launch_command(
        &self,
        agent_id: AgentId,
        default_command: &str,
        context: &AgentSessionWorkspaceContext,
        resume_provider_session: bool,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<(String, String, Vec<String>)> {
        let command = if resume_provider_session {
            self.provider_resume_command(agent_id, context)
                .unwrap_or_else(|| default_command.to_owned())
        } else {
            default_command.to_owned()
        };
        let (shell, args) = agent_launch_command(&command);
        self.sync_agent_session_launch(context, &command, &shell, &args, options)?;
        Ok((command, shell, args))
    }

    /// 优先采用 provider 恢复能力；Pi 强制归一到原生命令，避免旧快照重启 adapter 参数。
    fn provider_resume_command(
        &self,
        agent_id: AgentId,
        context: &AgentSessionWorkspaceContext,
    ) -> Option<String> {
        // Older provider.toml files may have been written while a provider was
        // marked resumable but had no concrete command. Keep those sessions
        // resumable by filling only the missing command from the current
        // provider defaults; an explicit `resume_supported = false` remains a
        // deliberate opt-out.
        let defaults = AgentProviderSession::for_agent(agent_id);
        let provider = self
            .read_agent_provider_session(agent_id, context)
            .unwrap_or_else(|| defaults.clone());
        if !provider.resume_supported {
            return None;
        }
        if agent_id == AgentId::Pi {
            return Some(PI_AGENT_RESUME_COMMAND.to_owned());
        }
        provider
            .resume_command
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| {
                defaults
                    .resume_command
                    .filter(|command| !command.trim().is_empty())
            })
    }

    /// 读取且校验 provider.toml；损坏或 provider 不匹配时由上层使用当前默认值恢复。
    fn read_agent_provider_session(
        &self,
        agent_id: AgentId,
        context: &AgentSessionWorkspaceContext,
    ) -> Option<AgentProviderSession> {
        let contents = fs::read_to_string(context.session_root.join("provider.toml")).ok()?;
        let provider = toml::from_str::<AgentProviderSession>(&contents).ok()?;
        provider.validate().ok()?;
        if provider.provider != AgentProvider::from(agent_id) {
            return None;
        }
        Some(provider)
    }

    /// 原子更新已有 session 的 launch 快照；dry-run 或兼容性无 session 调用不产生写入。
    fn sync_agent_session_launch(
        &self,
        context: &AgentSessionWorkspaceContext,
        command_label: &str,
        shell: &str,
        args: &[String],
        options: &WorkspaceWriteOptions,
    ) -> AppResult<()> {
        if options.dry_run {
            return Ok(());
        }

        let store = AgentSessionFileStore::new(&self.workspace_dir);
        let agent_session_id = AgentSessionId::new(context.agent_session_id.clone())?;
        let mut session = match store.read_session(&agent_session_id) {
            Ok(session) => session,
            Err(AppError::Io(error)) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        session.launch.command_label = command_label.to_owned();
        session.launch.shell = shell.to_owned();
        session.launch.args = args.to_vec();
        session.launch.cwd = path_to_string(&context.session_root);
        store.write_session(&session)?;
        Ok(())
    }

    /// 从 session.toml 读取 Custom 启动标题，使定义重命名只影响新会话，历史会话继续
    /// 使用创建时快照；未创建 session 文件的兼容调用仍回退到通用标题。
    fn custom_agent_session_title(
        &self,
        context: &AgentSessionWorkspaceContext,
    ) -> AppResult<String> {
        let store = AgentSessionFileStore::new(&self.workspace_dir);
        let agent_session_id = AgentSessionId::new(context.agent_session_id.clone())?;
        match store.read_session(&agent_session_id) {
            Ok(session) => {
                let title = session.title.trim();
                Ok(if title.is_empty() {
                    "Custom Agent".to_owned()
                } else {
                    title.to_owned()
                })
            }
            Err(AppError::Io(error)) if error.kind() == ErrorKind::NotFound => {
                Ok("Custom Agent".to_owned())
            }
            Err(error) => Err(error),
        }
    }

    /// 校验 provider/session id 并构造限定在该 session 目录和 MCP endpoint 的上下文。
    fn agent_session_context(
        &self,
        agent_id: &str,
        agent_session_id: &str,
    ) -> AppResult<AgentSessionWorkspaceContext> {
        let agent_session_id = validate_agent_session_id(agent_session_id)?;
        match agent_id {
            "codex" | "claude" | "pi" | "custom" => {}
            other => {
                return Err(AppError::InvalidInput(format!(
                    "Unsupported external agent: {other}"
                )))
            }
        };
        Ok(AgentSessionWorkspaceContext {
            agent_id: agent_id.to_owned(),
            agent_session_id: agent_session_id.clone(),
            session_root: self.agent_session_root(&agent_session_id),
            mcp_endpoint: scoped_agent_mcp_endpoint(&self.mcp_endpoint, &agent_session_id),
        })
    }

    /// 使用经校验的 session id 构造固定目录，避免 provider 自行选择 cwd。
    fn agent_session_root(&self, agent_session_id: &str) -> PathBuf {
        self.workspace_dir
            .join("agents")
            .join("sessions")
            .join(agent_session_id)
    }

    /// 只注入 Kerminal session 所需的最小环境，不复制或记录用户凭据。
    fn agent_session_env(
        &self,
        context: &AgentSessionWorkspaceContext,
    ) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "KERMINAL_AGENT_SESSION_ID".to_owned(),
                context.agent_session_id.clone(),
            ),
            (
                "KERMINAL_WORKSPACE_ROOT".to_owned(),
                path_to_string(&self.workspace_dir),
            ),
            (
                "KERMINAL_AGENT_SESSION_ROOT".to_owned(),
                path_to_string(&context.session_root),
            ),
            (
                "KERMINAL_MCP_ENDPOINT".to_owned(),
                context.mcp_endpoint.clone(),
            ),
        ])
    }
}
