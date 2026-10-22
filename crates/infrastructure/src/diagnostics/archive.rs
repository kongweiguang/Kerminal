//! 诊断报告的私有 ZIP 归档与原子提交。
//!
//! @author kongweiguang

use std::{
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use kerminal_application::{
    AppError, AppResult,
    diagnostics::{
        DiagnosticBundle, DiagnosticBundleReport, DiagnosticSecurityPolicy, redact_diagnostic_text,
    },
};
use tokio::task;
use uuid::Uuid;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use super::LocalDiagnosticsAdapter;
use crate::storage::{AtomicCommitError, CommitMode, StorageCoordinator, commit_staged_file};

/// 把报告序列化和压缩移出 async worker，归档过程只接收已完成白名单投影的业务 DTO。
pub(super) async fn create(
    adapter: &LocalDiagnosticsAdapter,
    report: DiagnosticBundleReport,
) -> AppResult<DiagnosticBundle> {
    let storage = adapter.storage.clone();
    task::spawn_blocking(move || create_blocking(&storage, report))
        .await
        .map_err(|error| AppError::infrastructure_with_source("无法创建诊断支持包", error))?
}

/// 锁定共享数据根后，在固定目录中创建独占临时文件并以不覆盖语义提交 ZIP。
fn create_blocking(
    storage: &StorageCoordinator,
    mut report: DiagnosticBundleReport,
) -> AppResult<DiagnosticBundle> {
    let _storage_lock = storage
        .lock()
        .map_err(|error| AppError::infrastructure_with_source("无法取得数据目录锁", error))?;
    let root = storage.root();
    let directory = ensure_diagnostics_directory(root)?;
    let id = Uuid::new_v4().simple().to_string();
    let created_at = timestamp_for_file_name();
    report.created_at = created_at.clone();
    let file_name = format!("kerminal-diagnostics-{created_at}-{id}.zip");
    let sections = report.sections.clone();
    let bytes = serialize_sanitized_report(report)?;
    let staging_path = directory.join(format!(".{file_name}.staging"));
    let destination = directory.join(&file_name);
    let staging_file = create_staging_file(&staging_path)?;

    let archive_bytes = match write_archive(staging_file, &bytes) {
        Ok(bytes) => bytes,
        Err(write_error) => {
            return Err(cleanup_after_failure(&staging_path, write_error));
        }
    };

    match commit_staged_file(&staging_path, &destination, CommitMode::CreateNew) {
        Ok(()) => Ok(DiagnosticBundle {
            id,
            created_at,
            file_name: file_name.clone(),
            relative_path: format!("diagnostics/{file_name}"),
            bytes_written: archive_bytes,
            sections,
            redacted: true,
        }),
        Err(AtomicCommitError::NotCommitted(error)) => Err(cleanup_after_failure(
            &staging_path,
            AppError::infrastructure_with_source("诊断支持包提交失败", error),
        )),
        Err(AtomicCommitError::CommitUnknown(error)) => Err(AppError::outcome_unknown_with_source(
            "诊断支持包提交结果未知，请先检查诊断目录",
            error,
        )),
    }
}

/// 仅允许从现有普通数据目录创建 diagnostics 子目录，并拒绝符号链接目标。
fn ensure_diagnostics_directory(root: &Path) -> AppResult<PathBuf> {
    let root_metadata = fs::symlink_metadata(root)
        .map_err(|error| AppError::infrastructure_with_source("无法访问用户数据目录", error))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(AppError::invalid("用户数据目录不是普通目录"));
    }

    let directory = root.join("diagnostics");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(AppError::invalid("诊断目录不是普通目录"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_private_directory(&directory)
                .map_err(|error| AppError::infrastructure_with_source("无法创建诊断目录", error))?;
        }
        Err(error) => {
            return Err(AppError::infrastructure_with_source(
                "无法访问诊断目录",
                error,
            ));
        }
    }

    let metadata = fs::symlink_metadata(&directory)
        .map_err(|error| AppError::infrastructure_with_source("无法验证诊断目录", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::invalid("诊断目录不是普通目录"));
    }
    restrict_directory_permissions(&directory, metadata)
        .map_err(|error| AppError::infrastructure_with_source("无法保护诊断目录权限", error))?;
    Ok(directory)
}

/// 创建仅当前用户可访问的诊断目录，避免共享 umask 生成可被其他本机用户读取的支持包。
fn create_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

/// 收紧既有目录的权限，复用旧版 diagnostics 路径时也不留下组或其他用户可读的包。
fn restrict_directory_permissions(path: &Path, metadata: fs::Metadata) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mode = metadata.mode() & 0o777;
        if mode & 0o077 != 0 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, metadata);
        Ok(())
    }
}

/// 以 create_new 与 Unix 0600 权限打开 staging 文件，绝不跟随或覆盖既有路径。
fn create_staging_file(path: &Path) -> AppResult<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| AppError::infrastructure_with_source("无法创建诊断临时文件", error))
}

/// 将不可信自由文本作为 JSON 字符串逐项脱敏，并强制关闭历史原文类字段。
fn serialize_sanitized_report(mut report: DiagnosticBundleReport) -> AppResult<Vec<u8>> {
    force_security_policy(&mut report);
    let mut value = serde_json::to_value(report)
        .map_err(|error| AppError::infrastructure_with_source("无法编码诊断报告", error))?;
    sanitize_json_strings(&mut value);
    serde_json::to_vec_pretty(&value)
        .map_err(|error| AppError::infrastructure_with_source("无法编码诊断报告", error))
}

/// 由归档边界再次覆盖安全标记，避免未来调用方误将正文类信息声明为允许收集。
fn force_security_policy(report: &mut DiagnosticBundleReport) {
    report.security = DiagnosticSecurityPolicy {
        secret_redaction: true,
        raw_terminal_output_included: false,
        command_history_included: false,
        credential_values_included: false,
        log_content_included: false,
    };
    if let Some(logs) = &mut report.logs {
        logs.content_included = false;
    }
    if let Some(sessions) = &mut report.terminal_sessions {
        sessions.raw_output_included = false;
    }
}

/// 递归处理 JSON 的字符串值，不碰键名或结构，保证序列化结果始终是合法 JSON。
fn sanitize_json_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = redact_diagnostic_text(text),
        serde_json::Value::Array(values) => {
            for value in values {
                sanitize_json_strings(value);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                sanitize_json_strings(value);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

/// 将单份 JSON 写成压缩归档并同步 staging 文件，ZIP 中不加入日志、数据库或用户配置。
fn write_archive(file: File, contents: &[u8]) -> AppResult<u64> {
    let mut writer = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o600);
    writer
        .start_file("report.json", options)
        .map_err(|error| AppError::infrastructure_with_source("无法写入诊断 ZIP", error))?;
    writer
        .write_all(contents)
        .map_err(|error| AppError::infrastructure_with_source("无法写入诊断报告", error))?;
    let file = writer
        .finish()
        .map_err(|error| AppError::infrastructure_with_source("无法完成诊断 ZIP", error))?;
    file.sync_all()
        .map_err(|error| AppError::infrastructure_with_source("无法同步诊断 ZIP", error))?;
    file.metadata()
        .map(|metadata| metadata.len())
        .map_err(|error| AppError::infrastructure_with_source("无法读取诊断 ZIP 大小", error))
}

/// 确定未提交时清理 staging；清理本身失败时将临时文件残留作为可见基础设施错误返回。
fn cleanup_after_failure(staging_path: &Path, write_error: AppError) -> AppError {
    match fs::remove_file(staging_path) {
        Ok(()) => write_error,
        Err(error) if error.kind() == io::ErrorKind::NotFound => write_error,
        Err(cleanup_error) => AppError::infrastructure_with_source(
            "诊断写入失败且临时文件清理未完成",
            StagingCleanupFailure {
                write_error,
                cleanup_error,
            },
        ),
    }
}

/// 保留写入错误分类与清理错误链，但公开文案不携带 staging 路径。
#[derive(Debug)]
struct StagingCleanupFailure {
    write_error: AppError,
    cleanup_error: io::Error,
}

impl fmt::Display for StagingCleanupFailure {
    /// 清理错误只返回固定摘要，避免把临时文件路径带入 MCP 响应。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}；临时文件清理失败",
            self.write_error.public_code().as_str()
        )
    }
}

impl Error for StagingCleanupFailure {
    /// 原写入错误保留在结构体，底层清理错误进入内部错误链。
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.cleanup_error)
    }
}

/// 返回不含路径和主机信息的文件名时间部分。
fn timestamp_for_file_name() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".to_owned())
}

#[cfg(test)]
#[path = "../../tests/support/diagnostics_archive.rs"]
mod tests;
