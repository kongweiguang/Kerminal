//! 明确来源和目标的旧 Profile 离线导入入口。
//!
//! @author kongweiguang

use std::{
    ffi::{OsStr, OsString},
    io::{self, Write},
    path::PathBuf,
    sync::Arc,
};

use kerminal_application::data_migration::DataMigrationService;
use kerminal_infrastructure::data_migration::FileDataMigrationRepository;
use serde_json::{Value, json};

const COMMAND: &str = "--import-legacy-profiles";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImportMode {
    DryRun,
    Apply,
}

#[derive(Debug, Eq, PartialEq)]
struct ImportArguments {
    source: PathBuf,
    target: PathBuf,
    mode: ImportMode,
}

/// 仅首参命中导入命令时接管；参数和执行结果均输出单行脱敏 JSON。
pub(crate) fn run_from_args<I>(arguments: I) -> Option<i32>
where
    I: IntoIterator<Item = OsString>,
{
    let parsed = parse_arguments(arguments)?;
    let (exit, report) = match parsed {
        Ok(arguments) => execute(arguments),
        Err(()) => (
            2,
            json!({"ok":false,"code":"invalid_arguments","message":"用法：--import-legacy-profiles <source> --target <target> --dry-run|--apply"}),
        ),
    };
    if write_report(&report).is_err() {
        Some(2)
    } else {
        Some(exit)
    }
}

/// 两个根和执行模式均必须显式提供，避免拼写错误时误用真实默认用户目录。
fn parse_arguments<I>(arguments: I) -> Option<Result<ImportArguments, ()>>
where
    I: IntoIterator<Item = OsString>,
{
    let mut arguments = arguments.into_iter();
    if arguments.next()?.as_os_str() != OsStr::new(COMMAND) {
        return None;
    }
    let source = match arguments.next() {
        Some(source)
            if !source.is_empty()
                && source != "--target"
                && source != "--dry-run"
                && source != "--apply" =>
        {
            PathBuf::from(source)
        }
        _ => return Some(Err(())),
    };
    let mut target = None;
    let mut mode = None;
    while let Some(argument) = arguments.next() {
        if argument == "--target" {
            if target.is_some() {
                return Some(Err(()));
            }
            let Some(next) = arguments.next() else {
                return Some(Err(()));
            };
            if next.is_empty() || next == "--target" || next == "--dry-run" || next == "--apply" {
                return Some(Err(()));
            }
            target = Some(PathBuf::from(next));
        } else if argument == "--dry-run" || argument == "--apply" {
            if mode.is_some() {
                return Some(Err(()));
            }
            mode = Some(if argument == "--dry-run" {
                ImportMode::DryRun
            } else {
                ImportMode::Apply
            });
        } else {
            return Some(Err(()));
        }
    }
    Some(match (target, mode) {
        (Some(target), Some(mode)) => Ok(ImportArguments {
            source,
            target,
            mode,
        }),
        _ => Err(()),
    })
}

/// 预演与提交调用同一应用服务；公共错误只输出稳定代码和脱敏文案。
fn execute(arguments: ImportArguments) -> (i32, Value) {
    let repository = FileDataMigrationRepository::new(arguments.target);
    let service = DataMigrationService::new(Arc::new(repository));
    match arguments.mode {
        ImportMode::DryRun => match service.preview_legacy_profiles(&arguments.source) {
            Ok(preview) => (0, json!({"ok":true,"mode":"dry_run","preview":preview})),
            Err(error) => (
                1,
                json!({"ok":false,"mode":"dry_run","code":error.public_code().as_str(),"message":error.public_message()}),
            ),
        },
        ImportMode::Apply => match service.import_legacy_profiles(&arguments.source) {
            Ok(report) => (0, json!({"ok":true,"mode":"apply","report":report})),
            Err(error) => (
                1,
                json!({"ok":false,"mode":"apply","code":error.public_code().as_str(),"message":error.public_message()}),
            ),
        },
    }
}

/// stdout 仅输出结构化结果；凭据、源文件正文与底层错误链不进入控制台。
fn write_report(report: &Value) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, report).map_err(io::Error::other)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

#[cfg(test)]
#[path = "../tests/support/profile_import_unit.rs"]
mod tests;
