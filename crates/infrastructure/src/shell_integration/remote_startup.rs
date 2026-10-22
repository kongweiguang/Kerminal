//! 在 SSH exec channel 中启动默认交互 shell，并让 PTY stdin 只承载用户输入。
//!
//! @author kongweiguang

use kerminal_application::{AppError, AppResult, connections::RemoteShellKind};
use kerminal_domain::connections::RemoteShellDialect;

use super::{BASH_HOOK, FISH_HOOK, POWERSHELL_HOOK, ZSH_ENV, ZSH_HOOK, ZSH_PROFILE, ZSH_RC};

const MAX_REMOTE_STARTUP_COMMAND_BYTES: usize = 64 * 1024;
const POSIX_CLEANUP_FUNCTION: &str = "__kerminal_cleanup() {\n\
    __kerminal_status=$?\n\
    trap - 0\n\
    if ! rm -rf -- \"$__kerminal_dir\"; then\n\
      printf '%s\\n' 'Kerminal: private shell startup cleanup failed' >&2\n\
      if [ \"$__kerminal_status\" -eq 0 ]; then __kerminal_status=125; fi\n\
    fi\n\
    exit \"$__kerminal_status\"\n\
  }";

#[cfg(test)]
#[path = "../../tests/unit/remote_startup.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "../../tests/unit/remote_startup_pty.rs"]
mod pty_tests;

/// 交互 shell 的 exec 启动脚本；PTY 输入通道不再承载启动脚本或初始 cwd。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RemoteInteractiveStartup {
    pub(crate) exec_command: String,
    /// 该位只表示 hook 随已确认 exec 请求启动，不代表 OSC 133 已经到达。
    pub(crate) shell_integration_enabled: bool,
}

/// 按精确 shell kind 构造 exec 启动命令；未知 shell 保留原 request_shell 路径。
///
/// 当前 Bash 路径通过 ENV 启动文件先建立受控 hook，再以真实 login+interactive 方式加载
/// 用户配置。Zsh/Fish/PowerShell 同样把用户 cwd 和 hook 放到各自配置完成后的启动阶段。
/// 启动脚本由 exec channel 传输，避免 PTY canonical 行上限和启动时序竞争。
///
/// @author kongweiguang
pub(crate) fn remote_interactive_startup(
    kind: RemoteShellKind,
    dialect: RemoteShellDialect,
    working_directory: Option<&str>,
) -> AppResult<Option<RemoteInteractiveStartup>> {
    if working_directory.is_some_and(|directory| directory.contains('\0')) {
        return Err(AppError::invalid(
            "SSH terminal cwd contains an invalid character",
        ));
    }

    let Some(expected_dialect) = shell_kind_dialect(kind) else {
        if working_directory.is_some() {
            return Err(AppError::unsupported(
                "SSH shell identity is unknown; a working directory cannot be applied safely",
            ));
        }
        return Ok(None);
    };
    if dialect != RemoteShellDialect::Unknown && dialect != expected_dialect {
        return Err(AppError::unsupported(
            "SSH shell identity and trusted shell dialect do not match",
        ));
    }

    let exec_command = match kind {
        RemoteShellKind::Bash => bash_startup_command(working_directory),
        RemoteShellKind::Zsh => zsh_startup_command(working_directory),
        RemoteShellKind::Fish => fish_startup_command(working_directory),
        RemoteShellKind::PowerShell => powershell_startup_command(working_directory),
        RemoteShellKind::Other | RemoteShellKind::Unknown => unreachable!("unknown handled above"),
    };
    if exec_command.len() > MAX_REMOTE_STARTUP_COMMAND_BYTES {
        return Err(AppError::invalid(
            "SSH shell startup command exceeds the supported size",
        ));
    }

    Ok(Some(RemoteInteractiveStartup {
        exec_command,
        shell_integration_enabled: true,
    }))
}

/// shell kind 本身是 nonce 探测结果，足以映射其固定脚本语法；未知和其他 shell 不猜方言。
///
/// @author kongweiguang
fn shell_kind_dialect(kind: RemoteShellKind) -> Option<RemoteShellDialect> {
    match kind {
        RemoteShellKind::Bash | RemoteShellKind::Zsh => Some(RemoteShellDialect::Posix),
        RemoteShellKind::Fish => Some(RemoteShellDialect::Fish),
        RemoteShellKind::PowerShell => Some(RemoteShellDialect::PowerShell),
        RemoteShellKind::Other | RemoteShellKind::Unknown => None,
    }
}

/// 生成 Bash ENV 启动器，在 stdin 尚未交给 readline 前按原 login profile 顺序安装 hook。
///
/// @author kongweiguang
fn bash_startup_command(working_directory: Option<&str>) -> String {
    let cwd = working_directory.map_or_else(String::new, |directory| {
        format!("\ncd -- {} || exit 125\n", posix_quote(directory))
    });
    let env_script = format!(
        "set +o posix\nshopt -u inherit_errexit 2>/dev/null || :\nunset ENV\n\
         if shopt -q login_shell; then\n\
           if [ -r /etc/profile ]; then . /etc/profile; fi\n\
           for __kerminal_profile in \"$HOME/.bash_profile\" \"$HOME/.bash_login\" \"$HOME/.profile\"; do\n\
             if [ -r \"$__kerminal_profile\" ]; then . \"$__kerminal_profile\"; break; fi\n\
           done\n\
         fi\n\
         unset __kerminal_profile{cwd}\n{BASH_HOOK}"
    );
    posix_exec_wrapper(&format!(
        "__kerminal_write_script \"$__kerminal_dir/bash-env\" {} || exit 125\n\
         ENV=\"$__kerminal_dir/bash-env\" bash --posix --login -i\n\
         __kerminal_status=$?\nexit \"$__kerminal_status\"",
        posix_quote(&env_script)
    ))
}

/// 构造 Zsh 临时 ZDOTDIR wrappers，使 cwd 与 hook 在用户 .zlogin 之后安装。
///
/// @author kongweiguang
fn zsh_startup_command(working_directory: Option<&str>) -> String {
    let (zshenv, zprofile, zshrc, zlogin) = zsh_startup_scripts(working_directory);
    posix_exec_wrapper(&format!(
        "__kerminal_original_zdotdir=${{ZDOTDIR:-${{HOME:-}}}}\n\
         __kerminal_write_script \"$__kerminal_dir/.zshenv\" {} || exit 125\n\
         __kerminal_write_script \"$__kerminal_dir/.zprofile\" {} || exit 125\n\
         __kerminal_write_script \"$__kerminal_dir/.zshrc\" {} || exit 125\n\
         __kerminal_write_script \"$__kerminal_dir/.zlogin\" {} || exit 125\n\
         KERMINAL_ORIGINAL_ZDOTDIR=\"$__kerminal_original_zdotdir\" \\
         ZDOTDIR=\"$__kerminal_dir\" zsh -l -i\n\
         __kerminal_status=$?\nexit \"$__kerminal_status\"",
        posix_quote(&zshenv),
        posix_quote(&zprofile),
        posix_quote(&zshrc),
        posix_quote(&zlogin),
    ))
}

/// 在每个 Zsh 临时启动文件中先切回用户目录，再由共享包装脚本加载用户文件。
///
/// @author kongweiguang
fn zsh_startup_scripts(working_directory: Option<&str>) -> (String, String, String, String) {
    let zshenv = format!(
        "__kerminal_hook_zdotdir=$ZDOTDIR\n\
         __kerminal_user_zdotdir=${{KERMINAL_ORIGINAL_ZDOTDIR:-$HOME}}\n\
         unset KERMINAL_ORIGINAL_ZDOTDIR\n\
         ZDOTDIR=$__kerminal_user_zdotdir\n{ZSH_ENV}"
    );
    let zprofile = format!("ZDOTDIR=${{__kerminal_user_zdotdir:-$HOME}}\n{ZSH_PROFILE}");
    let zshrc = format!(
        "ZDOTDIR=${{__kerminal_user_zdotdir:-$HOME}}\n{ZSH_RC}\nZDOTDIR=$__kerminal_hook_zdotdir\n"
    );
    let cwd = working_directory.map_or_else(String::new, |directory| {
        format!("cd -- {} || exit 125\n", posix_quote(directory))
    });
    let zlogin = format!(
        "ZDOTDIR=${{__kerminal_user_zdotdir:-$HOME}}\n\
         if [[ -r \"$ZDOTDIR/.zlogin\" ]]; then source \"$ZDOTDIR/.zlogin\"; fi\n\
         __kerminal_user_zdotdir=${{ZDOTDIR:-$HOME}}\n\
         ZDOTDIR=$__kerminal_user_zdotdir\n{cwd}{ZSH_HOOK}"
    );
    (zshenv, zprofile, zshrc, zlogin)
}

/// Fish 的 --init-command 在用户 config.fish 后、交互输入前运行短 hook 与 cwd。
///
/// @author kongweiguang
fn fish_startup_command(working_directory: Option<&str>) -> String {
    let cwd = working_directory.map_or_else(String::new, |directory| {
        format!("cd -- {}; or exit 125; ", fish_quote(directory))
    });
    let init_command = format!("{cwd}source \"$KERMINAL_HOOK_DIR/fish.fish\"");
    let wrapper = format!(
        "__kerminal_write_script \"$__kerminal_dir/fish.fish\" {} || exit 125\n\
         KERMINAL_HOOK_DIR=\"$__kerminal_dir\" fish --login --interactive --init-command {}\n\
         __kerminal_status=$?\nexit \"$__kerminal_status\"",
        posix_quote(FISH_HOOK),
        posix_quote(&init_command),
    );
    fish_exec_wrapper(&wrapper)
}

/// PowerShell 保留 profiles，以 child -Login/-NoExit 执行 hook 和 cwd，并在 finally 清理私有目录。
///
/// @author kongweiguang
fn powershell_startup_command(working_directory: Option<&str>) -> String {
    let cwd = working_directory.map_or_else(String::new, |directory| {
        format!(
            "Set-Location -LiteralPath {} -ErrorAction Stop",
            powershell_quote(directory)
        )
    });
    let child_steps = if cwd.is_empty() {
        String::new()
    } else {
        format!("; {cwd}")
    };
    let child_prefix = powershell_quote("try { . ");
    let child_suffix = powershell_quote(&format!(
        "{child_steps}; }} catch {{ [Console]::Error.WriteLine($_.Exception.Message); exit 125 }}"
    ));
    let write_hook = powershell_quote(POWERSHELL_HOOK);
    format!(
        "$ErrorActionPreference = 'Stop'; \
         $__kerminal_temp_root = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()); \
         $__kerminal_separator = [string][IO.Path]::DirectorySeparatorChar; \
         if (-not $__kerminal_temp_root.EndsWith($__kerminal_separator, [System.StringComparison]::Ordinal)) {{ $__kerminal_temp_root += $__kerminal_separator }}; \
         $__kerminal_path_comparison = [System.StringComparison]::Ordinal; \
         if ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) {{ $__kerminal_path_comparison = [System.StringComparison]::OrdinalIgnoreCase }}; \
         $__kerminal_candidate = [IO.Path]::GetFullPath([IO.Path]::Combine($__kerminal_temp_root, ('kerminal-' + [guid]::NewGuid().ToString('N')))); \
         if (-not $__kerminal_candidate.StartsWith($__kerminal_temp_root, $__kerminal_path_comparison)) {{ throw 'private directory path escaped the temporary root' }}; \
         if ([IO.Directory]::Exists($__kerminal_candidate) -or [IO.File]::Exists($__kerminal_candidate)) {{ throw 'private directory name is already occupied' }}; \
         $__kerminal_dir = $null; $__kerminal_directory_created = $false; $__kerminal_cleanup_failed = $false; $__kerminal_status = $null; \
         try {{ \
           $__kerminal_directory_info = [IO.Directory]::CreateDirectory($__kerminal_candidate); \
           $__kerminal_dir = [IO.Path]::GetFullPath($__kerminal_directory_info.FullName); $__kerminal_directory_created = $true; \
           if (-not [string]::Equals($__kerminal_dir, $__kerminal_candidate, $__kerminal_path_comparison) -or -not $__kerminal_dir.StartsWith($__kerminal_temp_root, $__kerminal_path_comparison)) {{ throw 'private directory path validation failed' }}; \
           $__kerminal_directory_attributes = [IO.File]::GetAttributes($__kerminal_dir); \
           if (($__kerminal_directory_attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {{ throw 'private directory must not be a reparse point' }}; \
           if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {{ & chmod 700 -- $__kerminal_dir; if ($LASTEXITCODE -ne 0) {{ throw 'temporary directory permissions failed' }} }}; \
           [IO.File]::WriteAllText((Join-Path $__kerminal_dir 'powershell.ps1'), {write_hook}, [Text.UTF8Encoding]::new($false)); \
           $__kerminal_hook_literal = \"'\" + (Join-Path $__kerminal_dir 'powershell.ps1').Replace(\"'\", \"''\") + \"'\"; \
           $__kerminal_child_command = {child_prefix} + $__kerminal_hook_literal + {child_suffix}; \
           $__kerminal_executable = (Get-Process -Id $PID -ErrorAction Stop).Path; \
           if (-not $__kerminal_executable) {{ throw 'current shell executable path is unavailable' }}; \
           $__kerminal_args = @('-NoLogo', '-NoExit', '-Command', $__kerminal_child_command); \
           if ($PSVersionTable.PSVersion.Major -ge 6 -and [Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {{ $__kerminal_args = @('-Login') + $__kerminal_args }}; \
           $__kerminal_status = 0; \
           & $__kerminal_executable @__kerminal_args; \
           if ($null -ne $LASTEXITCODE) {{ $__kerminal_status = $LASTEXITCODE }}; \
         }} finally {{ \
           if ($__kerminal_directory_created) {{ \
             try {{ \
               $__kerminal_cleanup_path = [IO.Path]::GetFullPath($__kerminal_dir); \
               if (-not [string]::Equals($__kerminal_cleanup_path, $__kerminal_candidate, $__kerminal_path_comparison) -or -not $__kerminal_cleanup_path.StartsWith($__kerminal_temp_root, $__kerminal_path_comparison)) {{ throw 'private directory cleanup path validation failed' }}; \
               $__kerminal_cleanup_attributes = [IO.File]::GetAttributes($__kerminal_cleanup_path); \
               if (($__kerminal_cleanup_attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {{ throw 'private directory cleanup rejected a reparse point' }}; \
               [IO.Directory]::Delete($__kerminal_cleanup_path, $true); \
               if ([IO.Directory]::Exists($__kerminal_cleanup_path)) {{ throw 'private directory still exists after cleanup' }}; \
             }} catch {{ [Console]::Error.WriteLine('Kerminal: private shell startup cleanup failed'); $__kerminal_cleanup_failed = $true }} \
           }} \
         }}; \
         if ($__kerminal_cleanup_failed -and ($null -eq $__kerminal_status -or $__kerminal_status -eq 0)) {{ exit 125 }}; \
         if ($null -ne $__kerminal_status) {{ exit $__kerminal_status }}"
    )
}

/// POSIX exec wrapper 独占本次 mktemp 目录，并在交互 child 返回后通过 trap 清理该目录。
///
/// @author kongweiguang
fn posix_exec_wrapper(child_script: &str) -> String {
    format!(
        "/bin/sh -c {}",
        posix_quote(&posix_exec_wrapper_body(child_script))
    )
}

/// 构造仅清理本次私有目录的 POSIX 主体，供启动器共用并直接验证内部顺序。
///
/// @author kongweiguang
fn posix_exec_wrapper_body(child_script: &str) -> String {
    let wrapper = format!(
        "umask 077\n\
         __kerminal_dir=$(mktemp -d \"${{TMPDIR:-/tmp}}/kerminal.XXXXXXXXXX\") || exit 125\n\
         {POSIX_CLEANUP_FUNCTION}\n\
         __kerminal_write_script() {{ printf '%s' \"$2\" > \"$1\"; }}\n\
         trap '__kerminal_cleanup' 0\n\
         trap 'exit 129' HUP\ntrap 'exit 130' INT\ntrap 'exit 143' TERM\n\
         {child_script}"
    );
    wrapper
}

/// Fish 外层命令使用其单引号规则编码完整 POSIX wrapper，不把 shell 语法混作 PTY 输入。
///
/// @author kongweiguang
fn fish_exec_wrapper(child_script: &str) -> String {
    let wrapper = posix_exec_wrapper_body(child_script);
    format!("'/bin/sh' -c {}", fish_quote(&wrapper))
}

/// POSIX shell single-quote 仅允许关闭引号再编码单引号，阻止 cwd/script 逃出数据上下文。
///
/// @author kongweiguang
fn posix_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Fish literal 单引号规则对反斜杠和引号分别转义，不能复用 POSIX 引用函数。
///
/// @author kongweiguang
fn fish_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// PowerShell 单引号字面量用成对引号表示内部引号，不执行任意路径内容。
///
/// @author kongweiguang
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
