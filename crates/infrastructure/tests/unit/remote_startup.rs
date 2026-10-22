//! SSH shell exec 启动命令的无网络纯逻辑回归。
//!
//! @author kongweiguang

use kerminal_application::connections::RemoteShellKind;
use kerminal_domain::connections::RemoteShellDialect;

use super::{
    POSIX_CLEANUP_FUNCTION, posix_exec_wrapper_body, posix_quote, powershell_quote,
    remote_interactive_startup, zsh_startup_scripts,
};

/// Unknown/Other 继续使用原生 shell request，并且不会凭 cwd 猜测可用语法。
///
/// @author kongweiguang
#[test]
fn unknown_shell_falls_back_only_without_cwd() {
    for kind in [RemoteShellKind::Unknown, RemoteShellKind::Other] {
        assert!(
            remote_interactive_startup(kind, RemoteShellDialect::Unknown, None)
                .expect("unknown shell without cwd is valid")
                .is_none()
        );
        assert!(
            remote_interactive_startup(kind, RemoteShellDialect::Unknown, Some("/srv/work"),)
                .is_err()
        );
    }
}

/// Bash startup 不把命令写进 PTY，保留真实 login profile 并在 hook 前设置目标 cwd。
///
/// @author kongweiguang
#[test]
fn bash_exec_startup_keeps_login_profiles_and_quotes_cwd() {
    let startup = remote_interactive_startup(
        RemoteShellKind::Bash,
        RemoteShellDialect::Unknown,
        Some("/srv/user's work"),
    )
    .expect("Bash startup is valid")
    .expect("Bash kind has a managed startup");
    assert!(startup.shell_integration_enabled);
    let command = startup.exec_command;
    assert!(command.starts_with("/bin/sh -c "));
    assert!(command.contains("mktemp -d"));
    assert!(posix_exec_wrapper_body("").contains("trap '__kerminal_cleanup' 0"));
    assert!(POSIX_CLEANUP_FUNCTION.contains("if ! rm -rf -- \"$__kerminal_dir\"; then"));
    assert!(POSIX_CLEANUP_FUNCTION.contains("__kerminal_status=125"));
    assert!(POSIX_CLEANUP_FUNCTION.contains("exit \"$__kerminal_status\""));
    assert!(command.contains("ENV=\"$__kerminal_dir/bash-env\" bash --posix --login -i"));
    assert!(command.contains("set +o posix"));
    assert!(command.contains("/etc/profile"));
    assert!(command.contains("$HOME/.bash_profile"));
    assert!(command.contains("/srv/user"));
    assert!(command.contains("s work"));
    assert_eq!(
        posix_quote("cd -- '/srv/final' || exit 125"),
        "'cd -- '\\''/srv/final'\\'' || exit 125'"
    );
    assert!(command.contains("exit 125"));
    assert!(command.contains("__kerminal_finish_prompt"));
    assert!(!command.contains("printf '%b'"));
}

/// Zsh hook/cwd 位于用户 login profile 之后，避免 .zlogin 覆盖指定目标目录。
///
/// @author kongweiguang
#[test]
fn zsh_exec_startup_chains_user_zlogin_before_hook_and_cwd() {
    let startup = remote_interactive_startup(
        RemoteShellKind::Zsh,
        RemoteShellDialect::Posix,
        Some("/srv/final"),
    )
    .expect("Zsh startup is valid")
    .expect("Zsh kind has a managed startup");
    let command = startup.exec_command;
    let (zshenv, zprofile, zshrc, zlogin) = zsh_startup_scripts(Some("/srv/final"));

    assert!(command.contains("ZDOTDIR=\"$__kerminal_dir\" zsh -l -i"));
    assert!(
        zshenv.find("ZDOTDIR=$__kerminal_user_zdotdir").unwrap()
            < zshenv.find("source \"$ZDOTDIR/.zshenv\"").unwrap()
    );
    assert!(
        zprofile
            .find("ZDOTDIR=${__kerminal_user_zdotdir:-$HOME}")
            .unwrap()
            < zprofile.find("source \"$ZDOTDIR/.zprofile\"").unwrap()
    );
    let user_rc = zshrc.find("source \"$ZDOTDIR/.zshrc\"").unwrap();
    let hook_directory = zshrc.rfind("ZDOTDIR=$__kerminal_hook_zdotdir").unwrap();
    assert!(user_rc < hook_directory);

    let user_login = zlogin.find("source \"$ZDOTDIR/.zlogin\"").unwrap();
    let user_directory_update = zlogin
        .find("__kerminal_user_zdotdir=${ZDOTDIR:-$HOME}")
        .unwrap();
    let user_directory_restore = zlogin.find("ZDOTDIR=$__kerminal_user_zdotdir").unwrap();
    let cwd = zlogin.find("cd -- ").unwrap();
    let hook = zlogin.find("__kerminal_shell_integration_active").unwrap();
    assert!(user_login < user_directory_update);
    assert!(user_directory_update < user_directory_restore);
    assert!(user_directory_restore < cwd);
    assert!(cwd < hook);
}

/// Fish 的官方 init-command 时点在用户配置之后，命令直接走 exec 而不是 PTY 输入。
///
/// @author kongweiguang
#[test]
fn fish_exec_startup_runs_init_after_configuration() {
    let startup = remote_interactive_startup(
        RemoteShellKind::Fish,
        RemoteShellDialect::Fish,
        Some("/srv/a'b"),
    )
    .expect("Fish startup is valid")
    .expect("Fish kind has a managed startup");
    let command = startup.exec_command;

    assert!(command.contains("fish --login --interactive --init-command"));
    assert!(command.contains("KERMINAL_HOOK_DIR"));
    assert!(command.contains("/srv/a"));
    assert!(command.contains("or exit 125"));
}

/// PowerShell 以 login 参数启动 child 并保留用户 profile，不传 --NoProfile 或 PTY bootstrap。
///
/// @author kongweiguang
#[test]
fn powershell_exec_startup_preserves_login_profiles_and_cleanup() {
    let startup = remote_interactive_startup(
        RemoteShellKind::PowerShell,
        RemoteShellDialect::PowerShell,
        Some("C:\\work\\user's"),
    )
    .expect("PowerShell startup is valid")
    .expect("PowerShell kind has a managed startup");
    let command = startup.exec_command;

    assert!(command.contains("'-NoLogo', '-NoExit', '-Command'"));
    assert!(command.contains("@('-Login') + $__kerminal_args"));
    assert_eq!(powershell_quote("C:\\work\\user's"), "'C:\\work\\user''s'");
    assert!(command.contains("Set-Location -LiteralPath"));
    assert!(command.contains("-ErrorAction Stop"));
    assert!(command.contains("[IO.Directory]::CreateDirectory($__kerminal_candidate)"));
    assert!(command.contains("[System.StringComparison]::OrdinalIgnoreCase"));
    assert!(command.contains("[IO.Directory]::Delete($__kerminal_cleanup_path, $true)"));
    assert!(command.contains("Kerminal: private shell startup cleanup failed"));
    assert!(!command.contains("New-Item -ItemType Directory"));
    let cleanup = command
        .split("finally {")
        .nth(1)
        .expect("startup cleanup finally block is present")
        .split("};")
        .next()
        .expect("cleanup block closes before child status handling");
    assert!(!cleanup.contains("SilentlyContinue"));
    assert!(!command.contains("-NoProfile"));
}

/// 已探测 shell 与已有可信 dialect 冲突时拒绝生成语法不匹配的启动脚本。
///
/// @author kongweiguang
#[test]
fn mismatched_known_shell_and_dialect_is_unsupported() {
    assert!(
        remote_interactive_startup(RemoteShellKind::Bash, RemoteShellDialect::PowerShell, None,)
            .is_err()
    );
}

/// NUL 不可能作为远端文件名数据安全传入 shell，必须在生成 exec 字符串前拒绝。
///
/// @author kongweiguang
#[test]
fn cwd_with_nul_is_rejected() {
    assert!(
        remote_interactive_startup(
            RemoteShellKind::Bash,
            RemoteShellDialect::Posix,
            Some("/tmp/bad\0path"),
        )
        .is_err()
    );
}
