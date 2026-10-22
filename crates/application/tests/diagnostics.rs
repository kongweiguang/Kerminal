//! Diagnostics application service contract tests.
//!
//! @author kongweiguang

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kerminal_application::{
    AppResult,
    diagnostics::{
        DiagnosticAppMetadata, DiagnosticBundle, DiagnosticBundleArchive, DiagnosticBundleReport,
        DiagnosticCommandDatabase, DiagnosticConfigurationRecovery, DiagnosticContextSnapshot,
        DiagnosticEnvironment, DiagnosticPaths, DiagnosticSettings, DiagnosticSource,
        DiagnosticSourceStatus, DiagnosticTerminalSession, DiagnosticTerminalSessions,
        DiagnosticTerminalSettings, DiagnosticUnavailableReason, DiagnosticsContextProvider,
        DiagnosticsService, RuntimeGpuHealth, RuntimeHealthAvailability, RuntimeHealthCollector,
        RuntimeHealthSnapshot, RuntimeProcessHealth, RuntimeSamplingInfo, RuntimeStorageHealth,
        RuntimeSystemHealth,
    },
};

#[derive(Clone)]
struct StaticContextProvider(DiagnosticContextSnapshot);

#[async_trait]
impl DiagnosticsContextProvider for StaticContextProvider {
    /// 测试用 provider 返回一份固定白名单快照，避免访问真实用户配置目录。
    async fn collect(&self) -> AppResult<DiagnosticContextSnapshot> {
        Ok(self.0.clone())
    }
}

#[derive(Clone)]
struct StaticHealthCollector(RuntimeHealthSnapshot);

#[async_trait]
impl RuntimeHealthCollector for StaticHealthCollector {
    /// 测试用 collector 只提供虚构的进程和系统值，不读取宿主机运行状态。
    async fn collect(&self) -> AppResult<RuntimeHealthSnapshot> {
        Ok(self.0.clone())
    }
}

struct CapturingArchive(Arc<Mutex<Option<DiagnosticBundleReport>>>);

#[async_trait]
impl DiagnosticBundleArchive for CapturingArchive {
    /// 留存用例生成的报告用于断言安全策略与来源可用状态。
    async fn create(&self, report: DiagnosticBundleReport) -> AppResult<DiagnosticBundle> {
        let created_at = report.created_at.clone();
        *self.0.lock().expect("test mutex poisoned") = Some(report);
        Ok(DiagnosticBundle {
            id: "test-bundle".to_owned(),
            created_at,
            file_name: "test.zip".to_owned(),
            relative_path: "diagnostics/test.zip".to_owned(),
            bytes_written: 1,
            sections: Vec::new(),
            redacted: true,
        })
    }
}

/// 验证应用服务将来源缺失原因保留下来，并在归档前对自由文本再次脱敏。
#[tokio::test]
async fn create_bundle_keeps_unavailable_sources_and_redacts_runtime_text() {
    let reports = Arc::new(Mutex::new(None));
    let service = DiagnosticsService::new(
        Arc::new(StaticContextProvider(context_snapshot())),
        Arc::new(StaticHealthCollector(runtime_snapshot())),
        Arc::new(CapturingArchive(Arc::clone(&reports))),
    );

    let bundle = service
        .create_bundle()
        .await
        .expect("bundle creation succeeds");
    let report = reports
        .lock()
        .expect("test mutex poisoned")
        .clone()
        .expect("archive receives a report");

    assert_eq!(report.schema, "kerminal.diagnostics.v1");
    assert_eq!(
        report.availability.logs,
        DiagnosticSourceStatus::Unavailable(DiagnosticUnavailableReason::NotFound)
    );
    assert!(report.logs.is_none());
    assert_eq!(
        report.availability.settings,
        DiagnosticSourceStatus::Available
    );
    assert!(report.runtime_health.process.name.contains("[REDACTED]"));
    assert!(!report.runtime_health.process.name.contains("private-value"));
    assert!(report.runtime_health.redacted);
    assert!(report.security.secret_redaction);
    assert!(!report.security.raw_terminal_output_included);
    assert!(!report.security.command_history_included);
    assert!(!report.security.credential_values_included);
    assert!(!report.security.log_content_included);
    assert_eq!(bundle.relative_path, "diagnostics/test.zip");
}

/// 构造不关联文件系统的配置、设置和会话上下文样本。
fn context_snapshot() -> DiagnosticContextSnapshot {
    DiagnosticContextSnapshot {
        app: DiagnosticAppMetadata {
            name: "Kerminal".to_owned(),
            version: "0.4.0".to_owned(),
            rust_package: "kerminal-desktop".to_owned(),
        },
        environment: DiagnosticEnvironment {
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            family: "unix".to_owned(),
        },
        paths: DiagnosticPaths {
            root: "$KERMINAL_CONFIG_ROOT".to_owned(),
            command_database_file: "$KERMINAL_CONFIG_ROOT/data/command.sqlite".to_owned(),
            app_log_file: "$KERMINAL_CONFIG_ROOT/logs/kerminal.log".to_owned(),
            logs: "$KERMINAL_CONFIG_ROOT/logs".to_owned(),
            cache: "$KERMINAL_CONFIG_ROOT/cache".to_owned(),
            diagnostics: "$KERMINAL_CONFIG_ROOT/diagnostics".to_owned(),
        },
        logs: DiagnosticSource::Unavailable(DiagnosticUnavailableReason::NotFound),
        command_database: DiagnosticSource::Available(DiagnosticCommandDatabase {
            schema_version: Some(3),
            file_size_bytes: Some(512),
        }),
        settings: DiagnosticSource::Available(DiagnosticSettings {
            theme_mode: "dark".to_owned(),
            terminal: DiagnosticTerminalSettings {
                font_family: "Fira Code".to_owned(),
                font_size: 14,
                line_height: 1.2,
                cursor_blink: true,
                scrollback: 10_000,
            },
            keybinding_count: 8,
        }),
        terminal_sessions: DiagnosticSource::Available(DiagnosticTerminalSessions {
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
            raw_output_included: false,
        }),
        configuration_recovery: DiagnosticConfigurationRecovery {
            read_only: false,
            issue_count: 0,
        },
    }
}

/// 构造来源与指标均明确的健康快照，验证 0 值不会替代缺失来源。
fn runtime_snapshot() -> RuntimeHealthSnapshot {
    RuntimeHealthSnapshot {
        captured_at: "1792100000".to_owned(),
        process: RuntimeProcessHealth {
            pid: 100,
            name: "kerminal token=private-value".to_owned(),
            memory_bytes: Some(1024),
            virtual_memory_bytes: Some(4096),
            cpu_usage_percent: Some(1.0),
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
            global_cpu_usage_percent: Some(3.0),
            cpu_core_usage_percents: Some(vec![2.0, 4.0]),
            total_memory_bytes: Some(4096),
            used_memory_bytes: Some(2048),
            available_memory_bytes: Some(2048),
            total_swap_bytes: None,
            used_swap_bytes: None,
            uptime_seconds: Some(200),
            boot_time_seconds: Some(1792099800),
            gpus: Some(vec![RuntimeGpuHealth {
                name: "GPU".to_owned(),
                vendor: Some("Test".to_owned()),
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
            app_logs: DiagnosticSourceStatus::Unavailable(DiagnosticUnavailableReason::NotFound),
        },
        redacted: false,
    }
}
