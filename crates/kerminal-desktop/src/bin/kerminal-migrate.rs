//! 无窗口的数据格式维护入口；仅调用与普通启动相同的同步迁移用例。
//!
//! @author kongweiguang

#[path = "../bootstrap.rs"]
mod bootstrap;
#[path = "../config_root.rs"]
mod config_root;

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

/// console 独立处理错误并只写安全 JSON，不引入桌面 main 的消息框或运行态装配。
///
/// @author kongweiguang
fn main() -> ExitCode {
    let output = match run_command(std::env::args_os().skip(1)) {
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
    if write_output(&output.document).is_err() {
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
            if value.is_empty() || value == "--root" || value == "--dry-run" || value == "--apply" {
                return Err(AppError::invalid("迁移根目录缺少有效参数"));
            }
            root = Some(PathBuf::from(value));
        } else if argument == "--dry-run" || argument == "--apply" {
            if action.is_some() {
                return Err(AppError::invalid("必须且只能选择一种迁移模式"));
            }
            action = Some(if argument == "--dry-run" {
                MigrationAction::DryRun
            } else {
                MigrationAction::Apply
            });
        } else {
            return Err(AppError::invalid("迁移命令包含不支持的参数"));
        }
    }
    Ok(MigrationArguments {
        root,
        action: action.ok_or_else(|| AppError::invalid("必须显式指定 --dry-run 或 --apply"))?,
    })
}

/// inspect 直接保持只读，Blocked 仍返回完整安全检查结果但以失败退出；apply 复用启动屏障。
///
/// @author kongweiguang
fn run_command(arguments: impl IntoIterator<Item = OsString>) -> AppResult<CommandOutput> {
    let arguments = parse_migration_arguments(arguments)?;
    let explicit_root = arguments.root.is_some();
    let root = match arguments.root {
        Some(root) => root,
        None => config_root::ConfigRoot::resolve()
            .map(|root| root.as_path().to_path_buf())
            .map_err(|_| AppError::invalid("无法解析迁移数据目录"))?,
    };
    let legacy = bootstrap::legacy_window_state_path(explicit_root);
    let service = bootstrap::migration_service(&root, legacy.as_deref());
    match arguments.action {
        MigrationAction::DryRun => {
            let inspection = service.inspect()?;
            let success = !matches!(&inspection.readiness, DataMigrationReadiness::Blocked);
            Ok(CommandOutput {
                success,
                document: json!({"ok": success, "mode": "dry_run", "inspection": inspection}),
            })
        }
        MigrationAction::Apply => {
            bootstrap::run_after_migration(&service, |report| CommandOutput {
                success: true,
                document: json!({"ok": true, "mode": "apply", "report": report}),
            })
        }
    }
}

/// stdout 只承载一条 JSON 记录，原始错误链、配置路径和底层调试字串均不进入维护协议。
///
/// @author kongweiguang
fn write_output(document: &Value) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, document).map_err(io::Error::other)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

#[cfg(test)]
#[path = "../../tests/support/migration_arguments_unit.rs"]
mod tests;
