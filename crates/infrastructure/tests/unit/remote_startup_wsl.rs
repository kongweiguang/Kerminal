//! Windows 构建产物经 WSL 真实 PTY 执行，覆盖 checkout 换行对远端 shell 的影响。
//!
//! @author kongweiguang

use std::{
    io::Write,
    os::windows::process::CommandExt,
    process::{Command, Stdio},
};

use kerminal_application::connections::RemoteShellKind;
use kerminal_domain::connections::RemoteShellDialect;

use super::remote_interactive_startup;

/// 必须使用 Windows 编译时嵌入的脚本，而不是由 WSL 重新编译后的 LF 副本。
/// WSL 是显式验收前提；普通测试不隐式启动发行版，也不修改用户 shell 配置。
///
/// @author kongweiguang
#[test]
#[ignore = "requires WSL with Python 3, Bash, Zsh and Fish"]
fn windows_embedded_hooks_start_cleanly_in_linux_ptys() {
    for (shell, kind, dialect) in [
        ("bash", RemoteShellKind::Bash, RemoteShellDialect::Posix),
        ("zsh", RemoteShellKind::Zsh, RemoteShellDialect::Posix),
        ("fish", RemoteShellKind::Fish, RemoteShellDialect::Fish),
    ] {
        let startup = remote_interactive_startup(kind, dialect, Some("/tmp"))
            .expect("prepare production SSH startup")
            .expect("known shell has managed startup");
        let mut child = Command::new("wsl.exe")
            .creation_flags(0x0800_0000)
            .args(["--exec", "python3", "-", shell, &startup.exec_command])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start WSL PTY probe");
        child
            .stdin
            .take()
            .expect("probe stdin")
            .write_all(include_bytes!("../support/remote_startup_probe.py"))
            .expect("write isolated PTY probe");
        let output = child.wait_with_output().expect("finish WSL PTY probe");
        assert!(
            output.status.success(),
            "{shell} startup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "startup, command, exit and cleanup passed",
            "{shell} did not finish the PTY probe"
        );
    }
}
