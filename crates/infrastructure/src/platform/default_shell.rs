//! 从当前操作系统环境解析默认本地终端程序。
//!
//! @author kongweiguang

#[cfg(windows)]
use std::env;
use std::ffi::OsString;

use kerminal_application::{AppResult, preferences::DefaultLocalShellProvider};
use kerminal_domain::sessions::LocalSessionTarget;
#[cfg(unix)]
use portable_pty::CommandBuilder;

#[cfg(not(windows))]
use kerminal_application::AppError;
#[cfg(unix)]
use std::path::Path;

/// 只解析平台默认目标；显式 Profile 始终由 ProfileService 优先选择。
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemDefaultLocalShellProvider;

impl SystemDefaultLocalShellProvider {
    /// 创建无状态平台适配器，避免 UI 自行猜测当前系统的 shell 和参数。
    pub const fn new() -> Self {
        Self
    }
}

impl DefaultLocalShellProvider for SystemDefaultLocalShellProvider {
    /// 使用平台账户默认 shell 构造 PTY 目标，并将导出的 SHELL 与实际程序保持一致。
    ///
    /// @author kongweiguang
    fn resolve_default(&self) -> AppResult<LocalSessionTarget> {
        #[cfg(windows)]
        let (program, args) = windows_shell();

        #[cfg(unix)]
        let (program, args) = unix_shell()?;

        #[cfg(not(any(windows, unix)))]
        return Err(AppError::unsupported("当前平台没有默认本地 shell 适配器"));

        #[cfg(windows)]
        let environment: std::collections::BTreeMap<OsString, OsString> = Default::default();

        #[cfg(unix)]
        let mut environment: std::collections::BTreeMap<OsString, OsString> = Default::default();

        // 只有 Unix 默认 shell provider 能证明 program 本身就是账户 shell；Windows PowerShell
        // 不应伪装成 Unix 的 $SHELL，显式 Profile 仍由调用方保留其原始环境。
        #[cfg(unix)]
        environment.insert(OsString::from("SHELL"), program.clone());

        #[cfg(any(windows, unix))]
        Ok(LocalSessionTarget {
            program,
            args,
            working_directory: None,
            environment,
        })
    }
}

/// 保留既有 Windows PowerShell 默认入口，避免把 shell 选择混入 UI 或终端服务。
#[cfg(windows)]
fn windows_shell() -> (OsString, Vec<OsString>) {
    let system_root = env::var_os("SystemRoot").map(std::path::PathBuf::from);
    let system_powershell = system_root.map(|root| {
        root.join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe")
    });
    let program = system_powershell
        .filter(|path| path.is_file())
        .map(|path| path.into_os_string())
        .unwrap_or_else(|| OsString::from("powershell.exe"));
    (program, vec![OsString::from("-NoLogo")])
}

/// 复用 portable-pty 的账户 shell 解析逻辑；拒绝相对路径或目录后再查询 passwd shell。
///
/// @author kongweiguang
#[cfg(unix)]
fn unix_shell() -> AppResult<(OsString, Vec<OsString>)> {
    let mut builder = CommandBuilder::new_default_prog();
    let selected = builder.get_shell();
    if usable_unix_shell(&selected) {
        return Ok((OsString::from(selected), Vec::new()));
    }

    // get_shell() 会先读取 builder 的 SHELL 环境；清除无效值后才能确定地查 passwd。
    builder.env_remove("SHELL");
    let account_shell = builder.get_shell();
    if usable_unix_shell(&account_shell) {
        return Ok((OsString::from(account_shell), Vec::new()));
    }
    Err(AppError::unsupported("未找到可用的本地 shell"))
}

/// 只接受 portable-pty 账户解析器选出的绝对文件，避免从 GUI 工作目录启动相对 shell。
///
/// @author kongweiguang
#[cfg(unix)]
fn usable_unix_shell(shell: &str) -> bool {
    let path = Path::new(shell);
    path.is_absolute() && path.is_file()
}

#[cfg(test)]
#[path = "../../tests/unit/default_shell.rs"]
mod tests;
