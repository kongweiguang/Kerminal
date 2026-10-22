//! Windows 本地 PTY 的可执行发现环境适配。
//!
//! @author kongweiguang

use std::ffi::OsString;

use portable_pty::CommandBuilder;

/// 将应用进程实际选择的 PATH/PATHEXT 固定到 builder，避免 portable-pty 的注册表合并改变发现顺序。
///
/// 缺失值显式移除注册表合并结果；后续 target/startup 环境仍可按既定优先级覆盖此基线。
///
/// @author kongweiguang
pub(super) fn set_command_search_environment(
    command: &mut CommandBuilder,
    path: Option<OsString>,
    pathext: Option<OsString>,
) {
    match path {
        Some(path) => command.env("PATH", path),
        None => command.env_remove("PATH"),
    }
    match pathext {
        Some(pathext) => command.env("PATHEXT", pathext),
        None => command.env_remove("PATHEXT"),
    }
}
