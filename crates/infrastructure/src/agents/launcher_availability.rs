//! Agent Launcher 的只读 PATH 探测器；不会执行 CLI 或读取用户命令参数。
//!
//! @author kongweiguang

use std::{
    env,
    ffi::{OsStr, OsString},
    path::Path,
};

use kerminal_application::{AppError, AppResult, agents::AgentLauncherInstallationProbe};
use tokio_util::sync::CancellationToken;

/// 使用当前进程环境查找 builtin Agent CLI，不缓存 PATH 结果。
#[derive(Clone, Copy, Debug, Default)]
pub struct PathAgentLauncherInstallationProbe;

impl AgentLauncherInstallationProbe for PathAgentLauncherInstallationProbe {
    /// 固定命令只进行文件存在性检查；缺失 PATH 或文件返回 false，取消作为操作错误传播。
    fn is_installed(&self, command: &str, cancellation: &CancellationToken) -> AppResult<bool> {
        executable_on_path(
            command,
            env::var_os("PATH").as_deref(),
            env::var_os("PATHEXT").as_deref(),
            cancellation,
        )
    }
}

/// 与 main 的检测契约一致，仅按 PATH/PATHEXT 查找文件，不执行版本探测或 shell。
fn executable_on_path(
    command: &str,
    path_value: Option<&OsStr>,
    path_ext: Option<&OsStr>,
    cancellation: &CancellationToken,
) -> AppResult<bool> {
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled(
            "Agent launcher installation probe was cancelled",
        ));
    }
    if command.trim().is_empty() {
        return Ok(false);
    }
    let command_path = Path::new(command);
    if command_path.components().count() > 1 {
        return Ok(command_path.is_file());
    }
    let Some(path_value) = path_value else {
        return Ok(false);
    };
    let candidates = executable_names(command, path_ext);
    for directory in env::split_paths(path_value) {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled(
                "Agent launcher installation probe was cancelled",
            ));
        }
        for candidate in &candidates {
            if cancellation.is_cancelled() {
                return Err(AppError::cancelled(
                    "Agent launcher installation probe was cancelled",
                ));
            }
            if directory.join(candidate).is_file() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Windows 通过 PATHEXT 查找命令脚本和可执行文件，其它平台使用精确命令名。
fn executable_names(command: &str, path_ext: Option<&OsStr>) -> Vec<OsString> {
    #[cfg(windows)]
    {
        if Path::new(command).extension().is_some() {
            return vec![OsString::from(command)];
        }
        let extensions = path_ext
            .and_then(OsStr::to_str)
            .unwrap_or(".COM;.EXE;.BAT;.CMD");
        extensions
            .split(';')
            .filter(|extension| !extension.trim().is_empty())
            .map(|extension| OsString::from(format!("{command}{extension}")))
            .collect()
    }
    #[cfg(not(windows))]
    {
        let _ = path_ext;
        vec![OsString::from(command)]
    }
}

#[cfg(test)]
#[path = "../../tests/unit/agent_launcher_installation_probe.rs"]
mod tests;
