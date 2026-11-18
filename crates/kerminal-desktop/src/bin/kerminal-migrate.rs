//! 无窗口的数据格式维护入口；仅调用与普通启动相同的同步迁移用例。
//!
//! @author kongweiguang

#[path = "../bootstrap.rs"]
mod bootstrap;
#[path = "../config_root.rs"]
mod config_root;
#[path = "../plugin_catalogs.rs"]
mod plugin_catalogs;
#[path = "../profile_import.rs"]
mod profile_import;

use std::{
    ffi::OsString,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use kerminal_application::{AppError, AppResult, data_migration::DataMigrationReadiness};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MigrationAction {
    DryRun,
    #[cfg(windows)]
    InstallerDryRun,
    Apply,
}

#[derive(Debug, Eq, PartialEq)]
struct MigrationArguments {
    root: Option<PathBuf>,
    action: MigrationAction,
}

struct CommandOutput {
    success: bool,
    document: Value,
}

/// console 共用导入与迁移用例并只写安全 JSON，不引入桌面消息框或运行态装配。
///
/// @author kongweiguang
fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if let Some(exit_code) = profile_import::run_from_args(arguments.iter().cloned()) {
        return match exit_code {
            0 => ExitCode::SUCCESS,
            1 => ExitCode::FAILURE,
            _ => ExitCode::from(2),
        };
    }
    #[cfg(windows)]
    let installer_output = arguments
        .iter()
        .any(|argument| argument == "--installer-dry-run");
    #[cfg(not(windows))]
    let installer_output = false;
    let output = match run_command(arguments) {
        Ok(output) => output,
        Err(error) => CommandOutput {
            success: false,
            document: json!({
                "ok": false,
                "code": error.public_code().as_str(),
                "message": error.public_message(),
            }),
        },
    };
    let success = output.success;
    if write_output(&output.document, installer_output).is_err() {
        eprintln!("Kerminal migration report could not be written");
        return ExitCode::FAILURE;
    }
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// 必须显式选择只读检查或提交；根路径始终使用 OsString，避免中文、空格和非 UTF-8 被改写。
///
/// @author kongweiguang
fn parse_migration_arguments(
    arguments: impl IntoIterator<Item = OsString>,
) -> AppResult<MigrationArguments> {
    let mut arguments = arguments.into_iter();
    let mut root = None;
    let mut action = None;
    while let Some(argument) = arguments.next() {
        if argument == "--root" {
            if root.is_some() {
                return Err(AppError::invalid("迁移根目录只能指定一次"));
            }
            let value = arguments
                .next()
                .ok_or_else(|| AppError::invalid("迁移根目录缺少参数"))?;
            if value.is_empty()
                || value == "--root"
                || value == "--dry-run"
                || value == "--apply"
                || value == "--installer-dry-run"
            {
                return Err(AppError::invalid("迁移根目录缺少有效参数"));
            }
            root = Some(PathBuf::from(value));
        } else if argument == "--dry-run"
            || argument == "--apply"
            || argument == "--installer-dry-run"
        {
            if action.is_some() {
                return Err(AppError::invalid("必须且只能选择一种迁移模式"));
            }
            action = Some(match argument.to_str() {
                Some("--dry-run") => MigrationAction::DryRun,
                Some("--apply") => MigrationAction::Apply,
                #[cfg(windows)]
                Some("--installer-dry-run") => MigrationAction::InstallerDryRun,
                _ => return Err(AppError::invalid("迁移命令包含不支持的参数")),
            });
        } else {
            return Err(AppError::invalid("迁移命令包含不支持的参数"));
        }
    }
    let action = action.ok_or_else(|| AppError::invalid("必须显式指定迁移模式"))?;
    #[cfg(windows)]
    if action == MigrationAction::InstallerDryRun && root.is_some() {
        return Err(AppError::invalid("安装器预检不接受显式根目录"));
    }
    Ok(MigrationArguments { root, action })
}

/// inspect 直接保持只读，Blocked 仍返回完整安全检查结果但以失败退出；apply 复用启动屏障。
///
/// @author kongweiguang
fn run_command(arguments: impl IntoIterator<Item = OsString>) -> AppResult<CommandOutput> {
    let arguments = parse_migration_arguments(arguments)?;
    #[cfg(windows)]
    if arguments.action == MigrationAction::InstallerDryRun
        && let Some(root) = std::env::var_os("KERMINAL_CONFIG_ROOT")
        && !PathBuf::from(root).is_absolute()
    {
        return Err(AppError::invalid("安装器预检要求绝对配置根目录"));
    }
    let explicit_root = arguments.root.is_some();
    let root = match arguments.root {
        Some(root) => root,
        None => config_root::ConfigRoot::resolve()
            .map(|root| root.as_path().to_path_buf())
            .map_err(|_| AppError::invalid("无法解析迁移数据目录"))?,
    };
    let legacy = bootstrap::legacy_window_state_path(explicit_root);
    let (_, host_types) = plugin_catalogs::migration_catalogs(&root)?;
    let service = bootstrap::migration_service(&root, legacy.as_deref(), host_types);
    match arguments.action {
        MigrationAction::DryRun => {
            let inspection = service.inspect()?;
            let success = !matches!(&inspection.readiness, DataMigrationReadiness::Blocked);
            Ok(CommandOutput {
                success,
                document: json!({"ok": success, "mode": "dry_run", "inspection": inspection}),
            })
        }
        #[cfg(windows)]
        MigrationAction::InstallerDryRun => {
            let inspection = service.inspect()?;
            let success = !matches!(&inspection.readiness, DataMigrationReadiness::Blocked);
            Ok(CommandOutput {
                success,
                document: json!({"ok": success, "mode": "dry_run", "inspection": inspection}),
            })
        }
        MigrationAction::Apply => {
            bootstrap::run_after_migration(&service, false, |report| CommandOutput {
                success: true,
                document: json!({"ok": true, "mode": "apply", "report": report}),
            })
        }
    }
}

/// 普通 CLI 保持 UTF-8 协议；安装器专用结果带 UTF-16LE BOM，避免 NSIS 按系统代码页破坏中文。
///
/// @author kongweiguang
fn write_output(document: &Value, installer_output: bool) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    if installer_output {
        let json = serde_json::to_string(document).map_err(io::Error::other)?;
        stdout.write_all(&[0xff, 0xfe])?;
        for unit in json.encode_utf16().chain([u16::from(b'\n')]) {
            stdout.write_all(&unit.to_le_bytes())?;
        }
    } else {
        serde_json::to_writer(&mut stdout, document).map_err(io::Error::other)?;
        stdout.write_all(b"\n")?;
    }
    stdout.flush()
}

#[cfg(test)]
#[path = "../../tests/support/migration_arguments_unit.rs"]
mod tests;
