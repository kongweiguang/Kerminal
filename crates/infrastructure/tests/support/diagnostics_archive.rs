//! Diagnostics archive private-boundary integration tests.
//!
//! @author kongweiguang

use std::{
    fs::{self, File},
    io::Read,
    path::PathBuf,
};

use kerminal_application::diagnostics::{
    DiagnosticAppMetadata, DiagnosticBundleAvailability, DiagnosticBundleReport,
    DiagnosticCommandDatabase, DiagnosticConfigurationRecovery, DiagnosticEnvironment,
    DiagnosticLogs, DiagnosticPaths, DiagnosticSecurityPolicy, DiagnosticSettings,
    DiagnosticSourceStatus, DiagnosticTerminalSession, DiagnosticTerminalSessions,
    DiagnosticTerminalSettings, DiagnosticUnavailableReason, RuntimeGpuHealth,
    RuntimeHealthAvailability, RuntimeHealthSnapshot, RuntimeProcessHealth, RuntimeSamplingInfo,
    RuntimeStorageHealth, RuntimeSystemHealth,
};
use uuid::Uuid;
use zip::ZipArchive;

use crate::storage::StorageCoordinator;

use super::create_blocking;

struct TemporaryRoot(PathBuf);

impl TemporaryRoot {
    /// 每个测试独占一个新临时根，确保不会读取、覆盖或清理用户的正式配置数据。
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("kerminal-diagnostics-{}", Uuid::new_v4()));
        fs::create_dir(&root).expect("test root creation succeeds");
        Self(root)
    }
}

impl Drop for TemporaryRoot {
    /// 测试完成后只删除自身新建的临时根，避免共享目录残留归档样本。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 验证 ZIP 只有一份脱敏 JSON、使用固定相对路径并强制关闭敏感正文标记。
#[test]
fn archive_contains_only_a_sanitized_report() {
    let temporary_root = TemporaryRoot::new();
    let storage = StorageCoordinator::new(&temporary_root.0);
    let bundle = create_blocking(&storage, report()).expect("diagnostics archive succeeds");
    let archive_file = temporary_root.0.join(&bundle.relative_path);
    assert_eq!(
        bundle.relative_path,
        format!("diagnostics/{}", bundle.file_name)
    );
    assert_eq!(
        bundle.bytes_written,
        fs::metadata(&archive_file).unwrap().len()
    );
    assert_eq!(bundle.sections.len(), 8);

    let file = File::open(&archive_file).expect("archive file exists");
    let mut archive = ZipArchive::new(file).expect("archive is a valid ZIP");
    assert_eq!(archive.len(), 1);
    assert_eq!(
        archive
            .by_index(0)
            .expect("archive entry exists")
            .name()
            .expect("entry name is valid"),
        "report.json"
    );
    let mut entry = archive.by_name("report.json").expect("report entry exists");
    let mut text = String::new();
    entry
        .read_to_string(&mut text)
        .expect("report is UTF-8 JSON");
    assert!(!text.contains("archive-secret-value"));

    let report: serde_json::Value = serde_json::from_str(&text).expect("report is valid JSON");
    assert_eq!(report["security"]["secretRedaction"], true);
    assert_eq!(report["security"]["rawTerminalOutputIncluded"], false);
    assert_eq!(report["security"]["commandHistoryIncluded"], false);
    assert_eq!(report["security"]["credentialValuesIncluded"], false);
    assert_eq!(report["security"]["logContentIncluded"], false);
    assert_eq!(report["logs"]["contentIncluded"], false);
    assert_eq!(report["terminalSessions"]["rawOutputIncluded"], false);
    assert_eq!(report["availability"]["logs"]["reason"], "notFound");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let directory = temporary_root.0.join("diagnostics");
        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(archive_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

/// 验证既有 diagnostics 符号链接会被拒绝，归档不会跟随链接写出数据根。
#[cfg(unix)]
#[test]
fn archive_refuses_a_symlink_diagnostics_directory() {
    use std::os::unix::fs::symlink;

    let temporary_root = TemporaryRoot::new();
    let external_root = TemporaryRoot::new();
    symlink(&external_root.0, temporary_root.0.join("diagnostics"))
        .expect("test symlink creation succeeds");
    let storage = StorageCoordinator::new(&temporary_root.0);

    let result = create_blocking(&storage, report());

    assert!(result.is_err());
    assert_eq!(fs::read_dir(&external_root.0).unwrap().count(), 0);
}

/// 构造带有诱饵敏感值与不安全布尔标记的报告，测试归档边界防御而非真实配置。
fn report() -> DiagnosticBundleReport {
    let sections = vec![
        "app".to_owned(),
        "environment".to_owned(),
        "runtimeHealth".to_owned(),
        "paths".to_owned(),
        "logs".to_owned(),
        "commandDatabase".to_owned(),
        "settings".to_owned(),
        "terminalSessions".to_owned(),
    ];
    DiagnosticBundleReport {
        schema: "kerminal.diagnostics.v1".to_owned(),
        created_at: "1792100000".to_owned(),
        app: DiagnosticAppMetadata {
            name: "Kerminal token=archive-secret-value".to_owned(),
            version: "0.4.0".to_owned(),
            rust_package: "kerminal-desktop".to_owned(),
        },
        environment: DiagnosticEnvironment {
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            family: "unix".to_owned(),
        },
        runtime_health: RuntimeHealthSnapshot {
            captured_at: "1792100000".to_owned(),
            process: RuntimeProcessHealth {
                pid: 100,
                name: "Kerminal token=archive-secret-value".to_owned(),
                memory_bytes: Some(1024),
                virtual_memory_bytes: Some(4096),
                cpu_usage_percent: Some(0.0),
                started_at_seconds: Some(1792099900),
                uptime_seconds: Some(100),
                disk_read_bytes: Some(0),
                disk_written_bytes: Some(0),
            },
            system: RuntimeSystemHealth {
                os: "linux".to_owned(),
                arch: "x86_64".to_owned(),
                host_name: None,
                kernel_version: None,
                os_version: None,
                cpu_count: Some(2),
                global_cpu_usage_percent: Some(0.0),
                cpu_core_usage_percents: Some(vec![0.0, 0.0]),
                total_memory_bytes: Some(4096),
                used_memory_bytes: Some(1024),
                available_memory_bytes: Some(3072),
                total_swap_bytes: None,
                used_swap_bytes: None,
                uptime_seconds: Some(200),
                boot_time_seconds: Some(1792099800),
                gpus: Some(vec![RuntimeGpuHealth {
                    name: "GPU".to_owned(),
                    vendor: None,
                    driver_version: None,
                    memory_total_bytes: None,
                    memory_used_bytes: None,
                    utilization_percent: None,
                    temperature_celsius: None,
                }]),
            },
            storage: RuntimeStorageHealth {
                root: "$KERMINAL_CONFIG_ROOT".to_owned(),
                command_database_file: "$KERMINAL_CONFIG_ROOT/data/command.sqlite".to_owned(),
                app_log_file: "$KERMINAL_CONFIG_ROOT/logs/kerminal.log".to_owned(),
                logs: "$KERMINAL_CONFIG_ROOT/logs".to_owned(),
                diagnostics: "$KERMINAL_CONFIG_ROOT/diagnostics".to_owned(),
                root_size_bytes: Some(1024),
                command_database_file_size_bytes: Some(512),
                app_log_file_size_bytes: None,
                app_log_max_file_size_bytes: Some(1_000_000),
                app_log_rotation_keep_files: Some(5),
            },
            sampling: RuntimeSamplingInfo {
                source: "sysinfo::System".to_owned(),
                cpu_sample_interval_ms: 200,
                cpu_refreshed_twice: true,
            },
            availability: RuntimeHealthAvailability {
                process: DiagnosticSourceStatus::Available,
                system: DiagnosticSourceStatus::Available,
                gpu: DiagnosticSourceStatus::Unavailable(DiagnosticUnavailableReason::Unsupported),
                storage: DiagnosticSourceStatus::Available,
                command_database: DiagnosticSourceStatus::Available,
                app_logs: DiagnosticSourceStatus::Unavailable(
                    DiagnosticUnavailableReason::NotFound,
                ),
            },
            redacted: false,
        },
        paths: DiagnosticPaths {
            root: "$KERMINAL_CONFIG_ROOT".to_owned(),
            command_database_file: "$KERMINAL_CONFIG_ROOT/data/command.sqlite".to_owned(),
            app_log_file: "$KERMINAL_CONFIG_ROOT/logs/kerminal.log".to_owned(),
            logs: "$KERMINAL_CONFIG_ROOT/logs".to_owned(),
            cache: "$KERMINAL_CONFIG_ROOT/cache".to_owned(),
            diagnostics: "$KERMINAL_CONFIG_ROOT/diagnostics".to_owned(),
        },
        logs: Some(DiagnosticLogs {
            directory: "logs".to_owned(),
            active_file: "logs/kerminal.log".to_owned(),
            active_file_size_bytes: Some(128),
            max_file_size_bytes: Some(1_000_000),
            rotation_keep_files: Some(5),
            content_included: true,
        }),
        command_database: Some(DiagnosticCommandDatabase {
            schema_version: Some(3),
            file_size_bytes: Some(512),
        }),
        settings: Some(DiagnosticSettings {
            theme_mode: "dark".to_owned(),
            terminal: DiagnosticTerminalSettings {
                font_family: "font token=archive-secret-value".to_owned(),
                font_size: 14,
                line_height: 1.2,
                cursor_blink: true,
                scrollback: 10_000,
            },
            keybinding_count: 0,
        }),
        terminal_sessions: Some(DiagnosticTerminalSessions {
            total: 1,
            running: 1,
            exited: 0,
            items: vec![DiagnosticTerminalSession {
                target_kind: "local".to_owned(),
                owner_kind: "ui".to_owned(),
                lifecycle: "running".to_owned(),
                columns: 120,
                rows: 36,
                exit_code: None,
            }],
            raw_output_included: true,
        }),
        configuration_recovery: DiagnosticConfigurationRecovery {
            read_only: false,
            issue_count: 0,
        },
        availability: DiagnosticBundleAvailability {
            logs: DiagnosticSourceStatus::Unavailable(DiagnosticUnavailableReason::NotFound),
            command_database: DiagnosticSourceStatus::Available,
            settings: DiagnosticSourceStatus::Available,
            terminal_sessions: DiagnosticSourceStatus::Available,
        },
        security: DiagnosticSecurityPolicy {
            secret_redaction: false,
            raw_terminal_output_included: true,
            command_history_included: true,
            credential_values_included: true,
            log_content_included: true,
        },
        sections,
    }
}
