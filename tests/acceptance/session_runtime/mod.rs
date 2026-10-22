//! 真实 session runtime 验收编排入口。
//!
//! @author kongweiguang

mod scenarios;
mod session_io;
mod support;

use std::fs;
use std::sync::Arc;
use std::time::Instant;

use kerminal_application::sessions::SessionService;
use kerminal_infrastructure::local_pty::LocalPtySessionTransportFactory;
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use tokio::runtime::Handle;
use uuid::Uuid;

use scenarios::{
    SessionScenarioInputs, run_churn_scenario, run_count_scenario, run_mixed_scenario,
};
use support::arguments::parse_options;
use support::environment::{collect_environment, repository_root};
use support::measurements::{
    FailureRecord, RawOutputCounter, ResourceSampler, SampleRecorder, unix_time_ms,
};
use support::report::{AcceptanceReport, OptionRecord, write_artifacts};

/// 建立隔离运行目录、执行真实负载，并在退出前关闭所有 runner 会话。
///
/// @author kongweiguang
pub(super) async fn run() {
    let options = match parse_options() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("参数错误：{error}");
            std::process::exit(2);
        }
    };
    let run_id = Uuid::new_v4().simple().to_string();
    let root = repository_root();
    let output_dir = options.output_dir.clone().unwrap_or_else(|| {
        root.join("target")
            .join("acceptance")
            .join("session-runtime")
            .join(&run_id)
    });
    let temp_root = std::env::temp_dir().join(format!(
        "kerminal-session-acceptance-{}-{run_id}",
        std::process::id()
    ));
    if let Err(error) = fs::create_dir(&temp_root) {
        eprintln!("创建本次运行专属临时目录失败：{error}");
        std::process::exit(2);
    }
    let config_root = temp_root.join("configroot");
    let home_root = temp_root.join("home");
    let temp_child_root = temp_root.join("tmp");
    for path in [&config_root, &home_root, &temp_child_root] {
        if let Err(error) = fs::create_dir(path) {
            eprintln!("建立隔离 shell 目录失败：{error}");
            let _ = fs::remove_dir_all(&temp_root);
            std::process::exit(2);
        }
    }
    #[cfg(windows)]
    for path in [temp_root.join("appdata"), temp_root.join("local-appdata")] {
        if let Err(error) = fs::create_dir(path) {
            eprintln!("建立隔离 shell profile 目录失败：{error}");
            let _ = fs::remove_dir_all(&temp_root);
            std::process::exit(2);
        }
    }
    #[cfg(unix)]
    for path in [temp_root.join("xdg-config"), temp_root.join("xdg-cache")] {
        if let Err(error) = fs::create_dir(&path) {
            eprintln!("建立隔离 XDG 目录失败：{error}");
            let _ = fs::remove_dir_all(&temp_root);
            std::process::exit(2);
        }
    }

    let started_at = unix_time_ms();
    let source = collect_environment(&root);
    let recorded_options = OptionRecord::from_options(&options, &output_dir);
    let output_counter = Arc::new(RawOutputCounter::default());
    let factory = Arc::new(LocalPtySessionTransportFactory::new());
    let manager = Arc::new(
        SessionManager::new(factory, Handle::current(), SessionRuntimeConfig::default())
            .with_output_recorder(output_counter.clone()),
    );
    let service: Arc<dyn SessionService> = manager;
    let mut sampler = ResourceSampler::new();
    let mut report = AcceptanceReport {
        schema_version: 1,
        run_id: run_id.clone(),
        status: "running",
        started_at_utc_unix_ms: started_at,
        finished_at_utc_unix_ms: started_at,
        source,
        options: recorded_options,
        ui_frame_latency_ms: None,
        ui_frame_measurement: "not measured: this runner exercises SessionService and PTY only",
        notes: vec![
            "PTY response latency measures write-to-terminal-snapshot observation, not UI input or frame latency.".to_owned(),
            "PTY throughput counts bytes received by the real SessionService output reader, including any shell echo.".to_owned(),
            "The runner constructs no configuration, vault, or database adapter and passes a run-specific KERMINAL_CONFIG_ROOT to child shells.".to_owned(),
            "PowerShell probes use -NoProfile so the host user's profile is not loaded.".to_owned(),
        ],
        failures: Vec::new(),
        samples: Vec::new(),
        temporary_directory_cleanup: "pending".to_owned(),
        retained_temporary_directory: None,
    };

    let scenario_inputs = SessionScenarioInputs {
        service: &service,
        output_counter: output_counter.as_ref(),
        options: &options,
        temp_root: &temp_root,
        config_root: &config_root,
        home_root: &home_root,
        temp_child_root: &temp_child_root,
    };
    let run_started = Instant::now();
    for count in options.session_counts.iter().copied() {
        let result = {
            let mut recorder =
                SampleRecorder::new(&service, &mut sampler, &mut report, run_started);
            run_count_scenario(&scenario_inputs, &mut recorder, count).await
        };
        if let Err(error) = result {
            report.failures.push(FailureRecord {
                scenario: format!("sessions-{count}"),
                operation: "scenario".to_owned(),
                iteration: None,
                error,
            });
            break;
        }
    }
    if report.failures.is_empty()
        && let Err(error) = {
            let mut recorder =
                SampleRecorder::new(&service, &mut sampler, &mut report, run_started);
            run_churn_scenario(&scenario_inputs, &mut recorder).await
        }
    {
        report.failures.push(FailureRecord {
            scenario: "churn".to_owned(),
            operation: "scenario".to_owned(),
            iteration: None,
            error,
        });
    }
    if report.failures.is_empty()
        && !options.mixed_duration.is_zero()
        && let Err(error) = {
            let mut recorder =
                SampleRecorder::new(&service, &mut sampler, &mut report, run_started);
            run_mixed_scenario(&scenario_inputs, &mut recorder).await
        }
    {
        report.failures.push(FailureRecord {
            scenario: "mixed".to_owned(),
            operation: "scenario".to_owned(),
            iteration: None,
            error,
        });
    }

    let close_all_succeeded = match service.close_all().await {
        Ok(()) => true,
        Err(error) => {
            report.failures.push(FailureRecord {
                scenario: "cleanup".to_owned(),
                operation: "close-all".to_owned(),
                iteration: None,
                error: error.to_string(),
            });
            false
        }
    };
    let no_sessions_remain = match service.list(Default::default()).await {
        Ok(remaining) if remaining.is_empty() => true,
        Ok(remaining) => {
            report.failures.push(FailureRecord {
                scenario: "cleanup".to_owned(),
                operation: "list-after-close".to_owned(),
                iteration: None,
                error: format!(
                    "{} runner-owned session(s) remained after close_all",
                    remaining.len()
                ),
            });
            false
        }
        Err(error) => {
            report.failures.push(FailureRecord {
                scenario: "cleanup".to_owned(),
                operation: "list-after-close".to_owned(),
                iteration: None,
                error: error.to_string(),
            });
            false
        }
    };
    if close_all_succeeded && no_sessions_remain {
        match fs::remove_dir_all(&temp_root) {
            Ok(()) => report.temporary_directory_cleanup = "removed".to_owned(),
            Err(error) => {
                report.temporary_directory_cleanup = "failed".to_owned();
                report.retained_temporary_directory = Some(temp_root.display().to_string());
                report.failures.push(FailureRecord {
                    scenario: "cleanup".to_owned(),
                    operation: "temporary-directory".to_owned(),
                    iteration: None,
                    error: error.to_string(),
                });
            }
        }
    } else {
        report.temporary_directory_cleanup =
            "retained-because-session-cleanup-is-unconfirmed".to_owned();
        report.retained_temporary_directory = Some(temp_root.display().to_string());
    }
    report.finished_at_utc_unix_ms = unix_time_ms();
    report.status = if report.failures.is_empty() {
        "passed"
    } else {
        "failed"
    };
    let artifact_result = write_artifacts(&output_dir, &run_id, &report);
    if let Err(error) = artifact_result {
        eprintln!("保存验收报告失败：{error}");
        std::process::exit(2);
    }
    println!(
        "状态：{}；samples={}；failures={}；output={}",
        report.status,
        report.samples.len(),
        report.failures.len(),
        output_dir.display()
    );
    if !report.failures.is_empty() {
        std::process::exit(1);
    }
}
