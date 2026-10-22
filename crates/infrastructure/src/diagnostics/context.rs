//! 诊断支持包的配置、会话和文件状态采集。
//!
//! @author kongweiguang

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use kerminal_application::{
    AppError,
    diagnostics::{
        DiagnosticAppMetadata, DiagnosticCommandDatabase, DiagnosticConfigurationRecovery,
        DiagnosticContextSnapshot, DiagnosticEnvironment, DiagnosticLogs, DiagnosticPaths,
        DiagnosticSettings, DiagnosticSource, DiagnosticTerminalSession,
        DiagnosticTerminalSessions, DiagnosticUnavailableReason,
    },
    preferences::{SettingsSnapshot, ThemeMode},
    sessions::SessionFilter,
};
use kerminal_domain::{
    preferences::ConfigurationMode,
    sessions::{SessionLifecycle, SessionOwner, SessionTarget},
};
use tokio::task;

use super::{
    APP_LOG_FILE_NAME, APP_LOG_MAX_FILE_SIZE_BYTES, APP_LOG_ROTATION_KEEP_FILES,
    LocalDiagnosticsAdapter,
};

struct LocalFileSnapshot {
    database_size: Result<u64, DiagnosticUnavailableReason>,
    database_schema: Result<u32, DiagnosticUnavailableReason>,
    log_size: Result<u64, DiagnosticUnavailableReason>,
}

/// 并行读取现有 application services，并将本机文件访问移出异步 worker。
/// 文件采集失败用固定不可用状态继续生成诊断快照，不将本机路径或 I/O 错误泄露给导出内容。
///
/// @author kongweiguang
pub(super) async fn collect(
    adapter: &LocalDiagnosticsAdapter,
) -> kerminal_application::AppResult<DiagnosticContextSnapshot> {
    let (settings_result, session_result, file_result) = tokio::join!(
        adapter.preferences.load_settings(),
        adapter.sessions.list(SessionFilter::default()),
        collect_local_file_snapshot(adapter),
    );
    let file_snapshot = file_result.unwrap_or(LocalFileSnapshot {
        database_size: Err(DiagnosticUnavailableReason::CollectionFailed),
        database_schema: Err(DiagnosticUnavailableReason::CollectionFailed),
        log_size: Err(DiagnosticUnavailableReason::CollectionFailed),
    });

    let settings = match settings_result {
        Ok(versioned) if versioned.revision.is_some() => {
            DiagnosticSource::Available(settings_summary(versioned.settings))
        }
        Ok(_) => DiagnosticSource::Unavailable(DiagnosticUnavailableReason::NotFound),
        Err(error) => DiagnosticSource::Unavailable(unavailable_for_application_error(&error)),
    };
    let terminal_sessions = match session_result {
        Ok(sessions) => DiagnosticSource::Available(session_summary(sessions)),
        Err(_) => DiagnosticSource::Unavailable(DiagnosticUnavailableReason::CollectionFailed),
    };
    let command_database = match (file_snapshot.database_schema, file_snapshot.database_size) {
        (Ok(schema_version), Ok(file_size_bytes)) => {
            DiagnosticSource::Available(DiagnosticCommandDatabase {
                schema_version: Some(schema_version),
                file_size_bytes: Some(file_size_bytes),
            })
        }
        (Err(reason), _) | (_, Err(reason)) => DiagnosticSource::Unavailable(reason),
    };
    let logs = match file_snapshot.log_size {
        Ok(active_file_size_bytes) => DiagnosticSource::Available(DiagnosticLogs {
            directory: "logs".to_owned(),
            active_file: format!("logs/{APP_LOG_FILE_NAME}"),
            active_file_size_bytes: Some(active_file_size_bytes),
            max_file_size_bytes: Some(APP_LOG_MAX_FILE_SIZE_BYTES),
            rotation_keep_files: Some(APP_LOG_ROTATION_KEEP_FILES),
            content_included: false,
        }),
        Err(reason) => DiagnosticSource::Unavailable(reason),
    };
    let recovery = adapter.preferences.recovery_state();
    let root_label = diagnostic_root_label();

    Ok(DiagnosticContextSnapshot {
        app: DiagnosticAppMetadata {
            name: "Kerminal".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            rust_package: "kerminal-desktop".to_owned(),
        },
        environment: DiagnosticEnvironment {
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            family: std::env::consts::FAMILY.to_owned(),
        },
        paths: DiagnosticPaths {
            root: root_label.to_owned(),
            command_database_file: format!("{root_label}/data/command.sqlite"),
            app_log_file: format!("{root_label}/logs/{APP_LOG_FILE_NAME}"),
            logs: format!("{root_label}/logs"),
            cache: format!("{root_label}/cache"),
            diagnostics: format!("{root_label}/diagnostics"),
        },
        logs,
        command_database,
        settings,
        terminal_sessions,
        configuration_recovery: DiagnosticConfigurationRecovery {
            read_only: recovery.mode == ConfigurationMode::ReadOnlyRecovery,
            issue_count: recovery.issues.len(),
        },
    })
}

/// 把常见错误映射成固定分类；诊断报告不保留错误源文本或用户文件路径。
fn unavailable_for_application_error(error: &AppError) -> DiagnosticUnavailableReason {
    match error.public_code() {
        kerminal_application::error::PublicErrorCode::ConfigurationCorrupt => {
            DiagnosticUnavailableReason::InvalidData
        }
        kerminal_application::error::PublicErrorCode::NotFound => {
            DiagnosticUnavailableReason::NotFound
        }
        kerminal_application::error::PublicErrorCode::CapabilityNotSupported
        | kerminal_application::error::PublicErrorCode::Unsupported => {
            DiagnosticUnavailableReason::Unsupported
        }
        _ => DiagnosticUnavailableReason::CollectionFailed,
    }
}

/// 抽出设置页承诺可编辑的字段，故意忽略任意扩展 TOML 与全部快捷键内容。
fn settings_summary(settings: SettingsSnapshot) -> DiagnosticSettings {
    DiagnosticSettings {
        theme_mode: match settings.theme_mode {
            ThemeMode::Dark => "dark",
            ThemeMode::Light => "light",
            ThemeMode::System => "system",
        }
        .to_owned(),
        terminal: kerminal_application::diagnostics::DiagnosticTerminalSettings {
            font_family: settings.terminal.font_family,
            font_size: settings.terminal.font_size,
            line_height: settings.terminal.line_height,
            cursor_blink: settings.terminal.cursor_blink,
            scrollback: settings.terminal.scrollback,
        },
        keybinding_count: settings.keybindings.len(),
    }
}

/// 会话投影只把协议和 owner 映射为固定类别，不序列化会话 ID、目标参数或退出 signal。
fn session_summary(
    sessions: Vec<kerminal_domain::sessions::SessionInfo>,
) -> DiagnosticTerminalSessions {
    let items = sessions
        .into_iter()
        .map(|session| {
            let target_kind = match session.target {
                SessionTarget::Local(_) => "local",
                SessionTarget::Ssh(_) => "ssh",
                SessionTarget::Telnet(_) => "telnet",
                SessionTarget::Serial(_) => "serial",
                SessionTarget::Container(_) => "container",
            };
            let owner_kind = match session.owner {
                SessionOwner::Ui { .. } => "ui",
                SessionOwner::Agent { .. } => "agent",
                SessionOwner::Mcp => "mcp",
                SessionOwner::Service { .. } => "service",
            };
            let lifecycle = match session.lifecycle {
                SessionLifecycle::Starting => "starting",
                SessionLifecycle::Running => "running",
                SessionLifecycle::Closing => "closing",
                SessionLifecycle::Exited => "exited",
                SessionLifecycle::Closed => "closed",
                SessionLifecycle::Failed => "failed",
            };
            let exit_code = session.exit_status.and_then(|status| status.code);
            DiagnosticTerminalSession {
                target_kind: target_kind.to_owned(),
                owner_kind: owner_kind.to_owned(),
                lifecycle: lifecycle.to_owned(),
                columns: session.size.columns(),
                rows: session.size.rows(),
                exit_code,
            }
        })
        .collect::<Vec<_>>();
    let running = items
        .iter()
        .filter(|item| item.lifecycle == "running")
        .count();
    let exited = items.len().saturating_sub(running);

    DiagnosticTerminalSessions {
        total: items.len(),
        running,
        exited,
        items,
        raw_output_included: false,
    }
}

/// 在专用线程读取数据目录大小、SQLite 版本和活动日志文件状态。
async fn collect_local_file_snapshot(
    adapter: &LocalDiagnosticsAdapter,
) -> Result<LocalFileSnapshot, tokio::task::JoinError> {
    let root = adapter.storage.root().to_path_buf();
    let database_file = adapter.command_history.database_file().to_path_buf();
    let command_history = Arc::clone(&adapter.command_history);
    task::spawn_blocking(move || {
        let log_file = root.join("logs").join(APP_LOG_FILE_NAME);
        let database_size = file_size(&database_file);
        let database_schema = match database_size {
            Ok(_) => command_history
                .schema_version()
                .map_err(|_| DiagnosticUnavailableReason::CollectionFailed),
            Err(reason) => Err(reason),
        };
        LocalFileSnapshot {
            database_size,
            database_schema,
            log_size: file_size(&log_file),
        }
    })
    .await
}

/// 只统计普通文件和目录，不跟随符号链接，读取任一目录失败时不声称得到完整大小。
pub(super) fn directory_size(root: &Path) -> Result<u64, DiagnosticUnavailableReason> {
    let root_metadata = fs::symlink_metadata(root).map_err(unavailable_for_io)?;
    if !root_metadata.is_dir() {
        return Err(DiagnosticUnavailableReason::InvalidData);
    }

    let mut total = 0_u64;
    let mut pending = vec![PathBuf::from(root)];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(directory).map_err(unavailable_for_io)?;
        for entry in entries {
            let entry = entry.map_err(unavailable_for_io)?;
            let file_type = entry.file_type().map_err(unavailable_for_io)?;
            if file_type.is_symlink() {
                return Err(DiagnosticUnavailableReason::InvalidData);
            } else if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                let metadata = entry.metadata().map_err(unavailable_for_io)?;
                total = total
                    .checked_add(metadata.len())
                    .ok_or(DiagnosticUnavailableReason::InvalidData)?;
            }
        }
    }
    Ok(total)
}

/// 只读取普通文件长度；链接、目录和不可读文件都作为不可用来源报告。
pub(super) fn file_size(path: &Path) -> Result<u64, DiagnosticUnavailableReason> {
    let metadata = fs::symlink_metadata(path).map_err(unavailable_for_io)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DiagnosticUnavailableReason::InvalidData);
    }
    Ok(metadata.len())
}

/// 把系统错误压缩为有限分类，避免序列化底层消息中的私有路径。
pub(super) fn unavailable_for_io(error: io::Error) -> DiagnosticUnavailableReason {
    match error.kind() {
        io::ErrorKind::NotFound => DiagnosticUnavailableReason::NotFound,
        io::ErrorKind::PermissionDenied => DiagnosticUnavailableReason::PermissionDenied,
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput => {
            DiagnosticUnavailableReason::InvalidData
        }
        _ => DiagnosticUnavailableReason::CollectionFailed,
    }
}

/// 根据配置根启动方式生成不含真实用户名的路径前缀。
pub(super) fn diagnostic_root_label() -> &'static str {
    if std::env::var_os("KERMINAL_CONFIG_ROOT").is_some() {
        "$KERMINAL_CONFIG_ROOT"
    } else {
        "~/.kerminal"
    }
}
