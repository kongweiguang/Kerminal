//! 本地与 SSH 交互 shell 的 OSC 133 bootstrap；不修改用户持久化配置。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use kerminal_application::{AppError, AppResult};
use kerminal_domain::{connections::RemoteShellDialect, sessions::LocalSessionTarget};
use uuid::Uuid;

mod remote_startup;
pub(crate) use remote_startup::remote_interactive_startup;

const BASH_HOOK: &str = include_str!("shell_integration/scripts/bash.sh");
const ZSH_ENV: &str = include_str!("shell_integration/scripts/zshenv.sh");
const ZSH_PROFILE: &str = include_str!("shell_integration/scripts/zprofile.sh");
const ZSH_RC: &str = include_str!("shell_integration/scripts/zshrc.sh");
const ZSH_HOOK: &str = include_str!("shell_integration/scripts/zsh_hook.sh");
const FISH_HOOK: &str = include_str!("shell_integration/scripts/fish.fish");
const POWERSHELL_HOOK: &str = include_str!("shell_integration/scripts/powershell.ps1");

/// 本地 PTY 使用的完整启动参数与临时脚本所有权。
///
/// `args` 包含调用方原参数；启动适配器只需将其与原 program 组合。guard 必须跟随子进程树，
/// 以免 shell 尚未读取 rc/bootstrap 文件时目录已被删除。
pub struct LocalShellStartupPlan {
    pub args: Vec<OsString>,
    pub env: BTreeMap<OsString, OsString>,
    pub dialect: RemoteShellDialect,
    pub lifetime_guard: Option<Box<dyn Send + Sync + 'static>>,
    pub bootstrap_status: ShellIntegrationBootstrapStatus,
    pub unavailable_reason: Option<ShellIntegrationUnavailableReason>,
}

/// 表示是否已准备可由子进程读取的 shell hook 文件；不代表提示符标记已到达。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellIntegrationBootstrapStatus {
    Prepared,
    Unsupported,
}

/// 未启用 shell hook 的静态原因，供会话状态展示，不包含路径或自由格式错误文本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellIntegrationUnavailableReason {
    UnsupportedShell,
    IncompatibleStartupArguments,
    MissingUserConfigDirectory,
    TemporaryFilesUnavailable,
}

/// 只为明确识别且可安全接管启动流程的 Bash、Zsh、Fish 与 PowerShell 交互 shell 安装 OSC 钩子。
///
/// 有显式命令或覆盖 hook 入口的用户启动参数时保留原 argv/env 并返回 Unknown；
/// PowerShell `-NoProfile` 只禁用用户 profile，不妨碍显式加载本应用的临时 hook。
/// 这样普通终端继续可用，但 workflow 不会把未观测到的 shell 状态伪装成成功。
///
/// @author kongweiguang
pub fn prepare_local_shell(target: &LocalSessionTarget) -> AppResult<LocalShellStartupPlan> {
    let Some(kind) = LocalShellKind::from_program(&target.program) else {
        return Ok(LocalShellStartupPlan::passthrough(
            target,
            ShellIntegrationUnavailableReason::UnsupportedShell,
        ));
    };
    if !kind.accepts_arguments(&target.args) {
        return Ok(LocalShellStartupPlan::passthrough(
            target,
            ShellIntegrationUnavailableReason::IncompatibleStartupArguments,
        ));
    }

    let original_zdotdir = if matches!(kind, LocalShellKind::Zsh) {
        match original_zdotdir(target) {
            Ok(path) => Some(path),
            Err(_) => {
                return Ok(LocalShellStartupPlan::passthrough(
                    target,
                    ShellIntegrationUnavailableReason::MissingUserConfigDirectory,
                ));
            }
        }
    } else {
        None
    };
    let temporary = match TemporaryShellFiles::create() {
        Ok(temporary) => temporary,
        Err(error) => {
            tracing::warn!(error_code = ?error.public_code(), "shell hook 临时目录不可用，保留原 shell 启动参数");
            return Ok(LocalShellStartupPlan::passthrough(
                target,
                ShellIntegrationUnavailableReason::TemporaryFilesUnavailable,
            ));
        }
    };
    let mut env_overrides = BTreeMap::new();
    let mut args = target.args.clone();
    let dialect = kind.dialect();

    let write_result = (|| -> AppResult<()> {
        match kind {
            LocalShellKind::Bash => {
                let rc_path = temporary.directory.join("bashrc");
                let user_rc = "if [ -n \"${HOME-}\" ] && [ -r \"$HOME/.bashrc\" ]; then\n  . \"$HOME/.bashrc\"\nfi\n";
                temporary.write_script(&rc_path, &format!("{user_rc}{BASH_HOOK}"))?;
                args.splice(0..0, [OsString::from("--rcfile"), rc_path.into_os_string()]);
            }
            LocalShellKind::Zsh => {
                let user_zdotdir = original_zdotdir.as_deref().unwrap_or_default();
                temporary.write_script(
                    &temporary.directory.join(".zshenv"),
                    &format!(
                        "{}{}",
                        zsh_wrapper_prelude(user_zdotdir, &temporary.directory, true),
                        ZSH_ENV
                    ),
                )?;
                temporary.write_script(
                    &temporary.directory.join(".zprofile"),
                    &format!(
                        "{}{}",
                        zsh_wrapper_prelude(user_zdotdir, &temporary.directory, false),
                        ZSH_PROFILE
                    ),
                )?;
                temporary.write_script(
                    &temporary.directory.join(".zshrc"),
                    &format!(
                        "{}{}{}",
                        zsh_wrapper_prelude(user_zdotdir, &temporary.directory, false),
                        ZSH_RC,
                        ZSH_HOOK
                    ),
                )?;
                env_overrides.insert(
                    OsString::from("ZDOTDIR"),
                    temporary.directory.as_os_str().to_owned(),
                );
            }
            LocalShellKind::Fish => {
                let hook_path = temporary.directory.join("fish.fish");
                temporary.write_script(&hook_path, FISH_HOOK)?;
                args.extend([
                    OsString::from("--init-command"),
                    OsString::from(format!("source {}", fish_quote(&hook_path)?)),
                ]);
            }
            LocalShellKind::PowerShell => {
                let hook_path = temporary.directory.join("powershell.ps1");
                temporary.write_script(&hook_path, POWERSHELL_HOOK)?;
                args.extend([
                    OsString::from("-NoExit"),
                    OsString::from("-Command"),
                    OsString::from(format!(". {}", powershell_quote(&hook_path)?)),
                ]);
            }
        }
        Ok(())
    })();
    if let Err(error) = write_result {
        tracing::warn!(error_code = ?error.public_code(), "shell hook 文件不可用，保留原 shell 启动参数");
        return Ok(LocalShellStartupPlan::passthrough(
            target,
            ShellIntegrationUnavailableReason::TemporaryFilesUnavailable,
        ));
    }

    Ok(LocalShellStartupPlan {
        args,
        env: env_overrides,
        dialect,
        lifetime_guard: Some(Box::new(temporary)),
        bootstrap_status: ShellIntegrationBootstrapStatus::Prepared,
        unavailable_reason: None,
    })
}

/// 只在目标 program 的实际 basename 与已实现启动方案完全匹配时返回 shell kind。
fn local_shell_kind(program: &OsStr) -> Option<LocalShellKind> {
    let name = Path::new(program)
        .file_stem()?
        .to_str()?
        .to_ascii_lowercase();
    match name.as_str() {
        "bash" => Some(LocalShellKind::Bash),
        "zsh" => Some(LocalShellKind::Zsh),
        "fish" => Some(LocalShellKind::Fish),
        "pwsh" | "powershell" => Some(LocalShellKind::PowerShell),
        _ => None,
    }
}

/// 以输入参数而非路径猜测判断该启动模式是否允许安装 rc/bootstrap 钩子。
/// PowerShell 的精确 `-NoProfile` 只禁用用户 profile；本应用仍可显式加载自己的临时 hook，
/// 但显式命令、脚本或非交互模式继续原样启动并保持 integration 未知。
///
/// @author kongweiguang
fn accepts_shell_arguments(kind: LocalShellKind, args: &[OsString]) -> bool {
    for argument in args {
        let Some(argument) = argument.to_str() else {
            return false;
        };
        let option = argument.to_ascii_lowercase();
        if kind == LocalShellKind::PowerShell && option == "-noprofile" {
            continue;
        }
        if !option.starts_with('-') {
            return false;
        }
        let forbidden = match kind {
            LocalShellKind::Bash => {
                option == "--"
                    || option.starts_with("--command")
                    || option.starts_with("--rcfile")
                    || option.starts_with("--init-file")
                    || option.starts_with("--norc")
                    || option.starts_with("--noprofile")
                    || option.starts_with("--login")
                    || option.starts_with("-c")
                    || (option.starts_with('-') && option[1..].contains('c'))
                    || (option.starts_with('-') && option[1..].contains('l'))
            }
            LocalShellKind::Zsh => {
                option == "--"
                    || option.starts_with("--command")
                    || option.starts_with("--no-rcs")
                    || option.starts_with("--no-global-rcs")
                    || (option.starts_with('-')
                        && option[1..]
                            .chars()
                            .any(|option| matches!(option, 'c' | 'f')))
            }
            LocalShellKind::Fish => {
                option == "--"
                    || option.starts_with("--command")
                    || option.starts_with("--init-command")
                    || option.starts_with("--no-config")
                    || option.starts_with("-c")
                    || option.starts_with("-n")
            }
            LocalShellKind::PowerShell => [
                "-command",
                "-c",
                "-file",
                "-f",
                "-encodedcommand",
                "-e",
                "-noninteractive",
                "-noprofile",
                "-configurationfile",
                "-settingsfile",
                "-inputformat",
            ]
            .iter()
            .any(|prefix| {
                option == *prefix
                    || option.starts_with(&format!("{prefix}:"))
                    || option.starts_with(&format!("{prefix}="))
            }),
        };
        if forbidden {
            return false;
        }
    }
    true
}

/// 读取用户当前 Zsh 配置目录；未知或非 UTF-8 路径时不接管启动目录。
fn original_zdotdir(target: &LocalSessionTarget) -> AppResult<String> {
    let target_value = target.environment.get(OsStr::new("ZDOTDIR"));
    let inherited_value = env::var_os("ZDOTDIR");
    let home_value = target
        .environment
        .get(OsStr::new("HOME"))
        .cloned()
        .or_else(|| env::var_os("HOME"));
    target_value
        .or(inherited_value.as_ref())
        .or(home_value.as_ref())
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            AppError::unsupported("无法安全确定 Zsh 用户配置目录，未启用 shell integration")
        })
}

/// 将 Zsh 配置 wrapper 指向用户目录并在文件读取期间保留临时 ZDOTDIR。
fn zsh_wrapper_prelude(
    original_zdotdir: &str,
    temporary_directory: &Path,
    initialize: bool,
) -> String {
    let original = shell_single_quote(original_zdotdir);
    let temporary = shell_single_quote(&temporary_directory.to_string_lossy());
    if initialize {
        format!(
            "# 先加载原用户启动文件，再返回临时目录读取后续 wrapper。\n__kerminal_hook_zdotdir={temporary}\n__kerminal_user_zdotdir={original}\nZDOTDIR=$__kerminal_user_zdotdir\n"
        )
    } else {
        "# 后续 wrapper 继承上一用户启动文件更新后的 ZDOTDIR。\nZDOTDIR=${__kerminal_user_zdotdir:-$HOME}\n".to_owned()
    }
}

/// 以 shell-safe 字面量表达临时目录中将会被 shell 读取的用户配置路径。
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 为 Fish 的 init command 生成只转义其单引号语法的绝对路径。
fn fish_quote(path: &Path) -> AppResult<String> {
    let path = path
        .to_str()
        .filter(|value| !value.chars().any(char::is_control))
        .ok_or_else(|| AppError::unsupported("Fish hook 路径不是安全的 UTF-8 路径"))?;
    Ok(format!(
        "'{}'",
        path.replace('\\', "\\\\").replace('\'', "\\'")
    ))
}

/// 生成 PowerShell 单引号字符串，路径内容不会被解释成表达式。
fn powershell_quote(path: &Path) -> AppResult<String> {
    let path = path
        .to_str()
        .filter(|value| !value.chars().any(char::is_control))
        .ok_or_else(|| AppError::unsupported("PowerShell hook 路径不是安全的 UTF-8 路径"))?;
    Ok(format!("'{}'", path.replace('\'', "''")))
}

/// 由唯一目录持有多份 shell startup 文件，并在进程树关闭后统一清理。
struct TemporaryShellFiles {
    directory: PathBuf,
}

impl TemporaryShellFiles {
    /// 用 UUID 与 create_dir/create_new 双重防碰撞创建私有临时配置目录。
    fn create() -> AppResult<Self> {
        let base = env::temp_dir();
        for _ in 0..8 {
            let directory = base.join(format!("kerminal-shell-{}", Uuid::new_v4()));
            match fs::create_dir(&directory) {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if let Err(source) =
                            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                        {
                            let _ = fs::remove_dir_all(&directory);
                            return Err(shell_file_error("设置 shell hook 目录权限失败", source));
                        }
                    }
                    return Ok(Self { directory });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(shell_file_error("创建 shell hook 临时目录失败", source));
                }
            }
        }
        Err(AppError::conflict("无法分配独立 shell hook 临时目录"))
    }

    /// 以独占创建和 fsync 写入脚本，避免跟随外部符号链接或读取半成品。
    fn write_script(&self, path: &Path, contents: &str) -> AppResult<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .map_err(|source| shell_file_error("创建 shell hook 文件失败", source))?;
        file.write_all(contents.as_bytes())
            .map_err(|source| shell_file_error("写入 shell hook 文件失败", source))?;
        file.sync_all()
            .map_err(|source| shell_file_error("同步 shell hook 文件失败", source))
    }
}

impl Drop for TemporaryShellFiles {
    /// 在 PTY 进程树释放其 guard 后清理独立脚本目录。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// 把文件系统错误转换成不包含临时路径的公开基础设施错误。
fn shell_file_error(context: &'static str, source: io::Error) -> AppError {
    AppError::infrastructure_with_source(context, source)
}

impl LocalShellStartupPlan {
    /// 未知 shell 或与用户显式启动约束冲突时完全保留原始启动语义。
    fn passthrough(target: &LocalSessionTarget, reason: ShellIntegrationUnavailableReason) -> Self {
        Self {
            args: target.args.clone(),
            env: BTreeMap::new(),
            dialect: RemoteShellDialect::Unknown,
            lifetime_guard: None,
            bootstrap_status: ShellIntegrationBootstrapStatus::Unsupported,
            unavailable_reason: Some(reason),
        }
    }
}

/// 本模块实际可 hook 的 shell，不以通用 POSIX 方言推断实现细节。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LocalShellKind {
    Bash,
    Zsh,
    Fish,
    PowerShell,
}

impl LocalShellKind {
    /// 只接受已实现 startup hook 的可执行文件名，不依据默认 shell 环境变量猜测。
    fn from_program(program: &OsStr) -> Option<Self> {
        local_shell_kind(program)
    }

    /// 将具体 shell 实现降为终端命令包装器实际需要的安全语法类别。
    fn dialect(self) -> RemoteShellDialect {
        match self {
            Self::Bash | Self::Zsh => RemoteShellDialect::Posix,
            Self::Fish => RemoteShellDialect::Fish,
            Self::PowerShell => RemoteShellDialect::PowerShell,
        }
    }

    /// 拒绝命令模式和禁用配置的 argv，避免把脚本 hook 加到一次性命令或无 rc shell。
    fn accepts_arguments(self, args: &[OsString]) -> bool {
        accepts_shell_arguments(self, args)
    }
}
