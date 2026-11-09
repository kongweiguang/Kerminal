//! 离线配置校验 CLI；不启动桌面 runtime 或配置仓储恢复流程。
//!
//! @author kongweiguang

use std::{
    ffi::{OsStr, OsString},
    io::{self, Write},
    path::{Path, PathBuf},
};

use kerminal_application::{
    connections::host_types::HostConfigurationCatalog, preferences::ConfigurationValidationScope,
};
use kerminal_infrastructure::configuration::ConfigurationStore;

const VALIDATE_CONFIG_ARGUMENT: &str = "--validate-config";
const INVALID_REPORT_EXIT_CODE: i32 = 1;
const COMMAND_ERROR_EXIT_CODE: i32 = 2;

/// 仅在命令行首参匹配时接管启动；保留原生参数以兼容 Windows 非 UTF-8 路径。
///
/// @author kongweiguang
pub(crate) fn run_from_args<I>(arguments: I) -> Option<i32>
where
    I: IntoIterator<Item = OsString>,
{
    match parse_root_argument(arguments) {
        None => None,
        Some(Err(())) => {
            eprintln!("用法：kerminal --validate-config <root>");
            Some(COMMAND_ERROR_EXIT_CODE)
        }
        Some(Ok(root)) => Some(match crate::plugin_catalogs::offline_catalogs(&root) {
            Ok((_, host_types)) => run_validation(&root, &host_types),
            Err(error) => {
                eprintln!("配置校验无法装配插件目录：{}", error.public_message());
                COMMAND_ERROR_EXIT_CODE
            }
        }),
    }
}

/// 接受一个显式根路径，避免 validator 静默改用默认用户目录或吞掉额外参数。
///
/// @author kongweiguang
fn parse_root_argument<I>(arguments: I) -> Option<Result<PathBuf, ()>>
where
    I: IntoIterator<Item = OsString>,
{
    let mut arguments = arguments.into_iter();
    let command = arguments.next()?;
    if command.as_os_str() != OsStr::new(VALIDATE_CONFIG_ARGUMENT) {
        return None;
    }

    let Some(root) = arguments.next() else {
        return Some(Err(()));
    };
    if root.is_empty() || arguments.next().is_some() {
        return Some(Err(()));
    }
    Some(Ok(PathBuf::from(root)))
}

/// 用冻结目录校验并输出 JSON；无效配置与执行失败保持不同退出码。
///
/// @author kongweiguang
fn run_validation(root: &Path, host_types: &HostConfigurationCatalog) -> i32 {
    let report = match ConfigurationStore::validate_root_with_host_types(
        root,
        ConfigurationValidationScope::All,
        host_types,
    ) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("配置校验无法完成：{error}");
            return COMMAND_ERROR_EXIT_CODE;
        }
    };

    let valid = report.valid;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = serde_json::to_writer_pretty(&mut output, &report) {
        eprintln!("无法输出配置校验报告：{error}");
        return COMMAND_ERROR_EXIT_CODE;
    }
    if let Err(error) = writeln!(output) {
        eprintln!("无法输出配置校验报告：{error}");
        return COMMAND_ERROR_EXIT_CODE;
    }

    if valid { 0 } else { INVALID_REPORT_EXIT_CODE }
}

#[cfg(test)]
#[path = "../tests/support/configuration_validation_unit.rs"]
mod tests;
