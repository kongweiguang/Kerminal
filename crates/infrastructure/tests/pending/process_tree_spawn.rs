//! process-wrap Tokio spawn adapter 的进程树回归测试。
//!
//! 该文件由 `tests/process_tree_spawn.rs` 显式引用，因此属于 Cargo 测试目标；进程树断言使用真实 Tokio wrapper。
//! @author kongweiguang

use std::{
    fs, io,
    path::PathBuf,
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::{
    io::AsyncReadExt,
    time::{Instant, sleep, timeout_at},
};

#[cfg(windows)]
use std::io::{Read, Write};

#[cfg(windows)]
const CONPTY_TEST_DRIVER_ENV: &str = "KERM_PROCESS_TREE_CONPTY_TEST_DRIVER";
#[cfg(windows)]
const CONPTY_TEST_DRIVER_RESULT_ENV: &str = "KERM_PROCESS_TREE_CONPTY_TEST_DRIVER_RESULT";
#[cfg(windows)]
const CONPTY_TEST_DRIVER_TRACE_ENV: &str = "KERM_PROCESS_TREE_CONPTY_TEST_DRIVER_TRACE";

#[path = "../support/process_tree_spawn_helper.rs"]
mod process_helper;

#[allow(
    dead_code,
    reason = "测试通过 path import 复用产品模块，但只覆盖其中一部分能力"
)]
#[path = "../../src/process_tree.rs"]
mod process_tree;

use process_tree::{ProcessTreeGuard, spawn::ProcessSpawnOptions};

/// 默认启动选项必须明确关闭 KillOnDrop，并避免附带平台创建 flags。
#[test]
fn default_options_do_not_request_kill_on_drop_or_creation_flags() {
    assert_eq!(
        ProcessSpawnOptions::default(),
        ProcessSpawnOptions {
            kill_on_drop: false,
            windows_creation_flags: 0,
        }
    );
}

/// 防止 Tokio Windows 调用方要求 breakaway 或挂起，绕开 spawn 时建立的 Job ownership。
#[cfg(windows)]
#[tokio::test]
async fn windows_tokio_spawn_rejects_breakaway_and_explicit_suspension_flags() {
    use windows::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_SUSPENDED};

    for flag in [CREATE_BREAKAWAY_FROM_JOB.0, CREATE_SUSPENDED.0] {
        let command = tokio::process::Command::new("process-that-must-not-spawn");
        let result = ProcessTreeGuard::spawn_tokio(
            command,
            ProcessSpawnOptions {
                kill_on_drop: false,
                windows_creation_flags: flag,
            },
        );
        let error = match result {
            Ok(_) => panic!("unsafe Windows process flag unexpectedly accepted"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}

/// 让 `current_exe` 能再次进入测试 harness，并按环境变量派生真实子孙进程。
#[test]
fn process_tree_spawn_helper() {
    process_helper::run_helper_mode().expect("process tree helper failed");
}

/// 验证 Unix process group 与 Windows spawn-time Job 都能结束立即派生的孙进程并关闭继承管道。
#[tokio::test]
async fn immediate_descendant_is_killed_and_inherited_pipe_closes() -> io::Result<()> {
    for _ in 0..8 {
        let deadline = Instant::now() + Duration::from_secs(5);
        let (mut child, pid_file) = spawn_root(true)?;
        let leaf_pid = wait_for_pid_file(&pid_file, deadline).await?;
        let stdout = child.stdout().take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "child stdout pipe is unavailable",
            )
        })?;
        let pipe_reader = tokio::spawn(async move {
            let mut output = Vec::new();
            let mut stdout = stdout;
            stdout.read_to_end(&mut output).await.map(|_| output)
        });

        child.terminate_tree_until(deadline).await?;
        timeout_at(deadline, child.wait()).await.map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "managed child wait timed out")
        })??;
        timeout_at(deadline, pipe_reader)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "inherited pipe remained open"))?
            .map_err(|error| io::Error::other(format!("pipe reader task failed: {error}")))??;
        wait_for_process_exit(leaf_pid, deadline).await?;
        let _ = fs::remove_file(pid_file);
    }
    Ok(())
}

/// 验证 Tokio KillOnDrop wrapper 关闭整个 job/group，而不是只回收直接 child。
#[tokio::test]
async fn kill_on_drop_stops_descendant_without_releasing_pipe_owner() -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut child, pid_file) = spawn_root(true)?;
    let leaf_pid = wait_for_pid_file(&pid_file, deadline).await?;
    let stdout = child.stdout().take().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "child stdout pipe is unavailable",
        )
    })?;
    let pipe_reader = tokio::spawn(async move {
        let mut output = Vec::new();
        let mut stdout = stdout;
        stdout.read_to_end(&mut output).await.map(|_| output)
    });

    drop(child);
    wait_for_process_exit(leaf_pid, deadline).await?;
    timeout_at(deadline, pipe_reader)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "drop left inherited pipe open"))?
        .map_err(|error| io::Error::other(format!("pipe reader task failed: {error}")))??;
    let _ = fs::remove_file(pid_file);
    Ok(())
}

/// 用受管外层进程给 ConPTY 测试 reader 提供硬期限；driver 从创建时起就在 kill-on-close Job 中。
///
/// @author kongweiguang
#[cfg(windows)]
#[tokio::test]
async fn prepared_windows_job_captures_immediate_pty_descendant() -> io::Result<()> {
    if std::env::var_os(CONPTY_TEST_DRIVER_ENV).is_some() {
        let result = run_prepared_windows_job_driver().await;
        record_conpty_driver_result(&result)?;
        return result;
    }

    run_prepared_windows_job_supervisor().await
}

/// 启动同一个精确测试作为隔离 driver；NUL 标准句柄避免捕获终端正文或形成额外管道。
///
/// driver 子进程由现有 ManagedTokioChild 原子加入 Job，超时清理只针对该拥有句柄。
///
/// @author kongweiguang
#[cfg(windows)]
async fn run_prepared_windows_job_supervisor() -> io::Result<()> {
    let result_path = temporary_pid_file().with_extension("driver-result");
    let trace_path = result_path.with_extension("driver-trace");
    let run_result = async {
        let executable = std::env::current_exe()?;
        let mut command = tokio::process::Command::new(executable);
        command
            .args([
                "--exact",
                "process_tree_spawn::prepared_windows_job_captures_immediate_pty_descendant",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CONPTY_TEST_DRIVER_ENV, "1")
            .env(CONPTY_TEST_DRIVER_RESULT_ENV, &result_path)
            .env(CONPTY_TEST_DRIVER_TRACE_ENV, &trace_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // 总期限先于 spawn 建立，预留最后两秒给受管 Job 回收，整个 supervisor 不超过十五秒。
        let total_deadline = Instant::now() + Duration::from_secs(15);
        let operation_deadline = total_deadline - Duration::from_secs(2);
        let mut driver = ProcessTreeGuard::spawn_tokio(
            command,
            ProcessSpawnOptions {
                kill_on_drop: true,
                windows_creation_flags: 0,
            },
        )?;

        loop {
            if let Some(status) = driver.try_wait()? {
                let diagnostic = match fs::read_to_string(&result_path) {
                    Ok(diagnostic) => diagnostic,
                    Err(error) => format!("driver_result=unavailable;kind={:?}", error.kind()),
                };
                let output_trace = match fs::read_to_string(&trace_path) {
                    Ok(trace) => trace,
                    Err(error) => format!("output_trace=unavailable;kind={:?}", error.kind()),
                };
                eprintln!("ConPTY fixed-token probe: {output_trace}");
                if status.success() && diagnostic == "status=passed" {
                    return Ok(());
                }
                return Err(io::Error::other(format!(
                    "ConPTY test driver failed (exit_code={:?}; {diagnostic})",
                    status.code()
                )));
            }

            if Instant::now() >= operation_deadline {
                let stage = fs::read_to_string(&result_path)
                    .unwrap_or_else(|error| format!("stage=unavailable;kind={:?}", error.kind()));
                return Err(
                    terminate_timed_out_conpty_driver(&mut driver, &stage, total_deadline).await,
                );
            }
            sleep(Duration::from_millis(10)).await;
        }
    }
    .await;
    let _ = fs::remove_file(result_path);
    let _ = fs::remove_file(trace_path);
    run_result
}

/// 只经独立 sidecar 回传测试阶段错误，不把 libtest 或终端正文接到外层输出管道。
///
/// @author kongweiguang
#[cfg(windows)]
fn record_conpty_driver_result(result: &io::Result<()>) -> io::Result<()> {
    let Some(path) = std::env::var_os(CONPTY_TEST_DRIVER_RESULT_ENV) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ConPTY test driver result path is missing",
        ));
    };
    let diagnostic = match result {
        Ok(()) => "status=passed".to_owned(),
        Err(error) => format!(
            "status=failed;kind={:?};raw_os_error={:?};detail={error}",
            error.kind(),
            error.raw_os_error()
        ),
    };
    fs::write(path, diagnostic)
}

/// 到硬期限后只终止并回收受管 driver；即使清理成功也必须把测试结果判为超时失败。
///
/// @author kongweiguang
#[cfg(windows)]
async fn terminate_timed_out_conpty_driver(
    driver: &mut process_tree::spawn::ManagedTokioChild,
    stage: &str,
    total_deadline: Instant,
) -> io::Error {
    let termination_error = driver.terminate_tree_until(total_deadline).await.err();
    let cleanup = termination_error.map_or_else(
        || "Job termination requested".to_owned(),
        |error| format!("Job termination error: {error}"),
    );

    loop {
        match driver.try_wait() {
            Ok(Some(status)) => {
                return io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "ConPTY test driver exceeded its hard deadline ({cleanup}; stage={stage}; exit_code={:?})",
                        status.code()
                    ),
                );
            }
            Ok(None) if Instant::now() < total_deadline => sleep(Duration::from_millis(10)).await,
            Ok(None) => {
                return io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "ConPTY test driver exceeded its hard deadline; exit not confirmed ({cleanup}; stage={stage})"
                    ),
                );
            }
            Err(error) => {
                return io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "ConPTY test driver exceeded its hard deadline; wait failed ({cleanup}; stage={stage}; error={error})"
                    ),
                );
            }
        }
    }
}

/// 写入固定测试阶段名称，便于外层 NUL 隔离的 supervisor 诊断超时停留位置。
///
/// @author kongweiguang
#[cfg(windows)]
fn record_conpty_driver_stage(stage: &'static str) -> io::Result<()> {
    let Some(path) = std::env::var_os(CONPTY_TEST_DRIVER_RESULT_ENV) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ConPTY test driver result path is missing",
        ));
    };
    fs::write(path, format!("stage={stage}"))
}

/// 在隔离 driver 内执行原 ConPTY 回归；PID 操作期限与有界清理期限分离，EOF 仍必须确认。
///
/// @author kongweiguang
#[cfg(windows)]
async fn run_prepared_windows_job_driver() -> io::Result<()> {
    use process_tree::PreparedWindowsJob;

    record_conpty_driver_stage("open-pty")?;
    let operation_deadline = Instant::now() + Duration::from_secs(5);
    // PID 发布仍限五秒；终止开始后另开两秒收尾窗，外层 supervisor 仍限十五秒。
    let mut cleanup_deadline = None;
    let pid_file = temporary_pid_file();
    let helper_stage_file = pid_file.with_extension("helper-stage");
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(io::Error::other)?;
    record_conpty_driver_stage("start-output-drain")?;
    let output_reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
    let input_writer = pair.master.take_writer().map_err(io::Error::other)?;
    let output_drain = ConPtyOutputDrain::start(output_reader, input_writer);
    let mut command = portable_pty::CommandBuilder::new(std::env::current_exe()?);
    command.args([
        "--exact",
        "process_tree_spawn::process_tree_spawn_helper",
        "--nocapture",
    ]);
    command.env(process_helper::PROCESS_HELPER_ROLE, "root");
    command.env(process_helper::PROCESS_HELPER_PID_FILE, &pid_file);
    command.env(
        process_helper::PROCESS_HELPER_STAGE_FILE,
        &helper_stage_file,
    );
    let helper_failure_file = pid_file.with_extension("helper-error");
    command.env(
        process_helper::PROCESS_HELPER_FAILURE_FILE,
        &helper_failure_file,
    );
    command.env(process_helper::PROCESS_HELPER_STDOUT_MODE, "null");

    let run_result = async {
        record_conpty_driver_stage("spawn-pty-helper")?;
        let job = PreparedWindowsJob::new()?;
        let mut child = job
            .spawn_command(&*pair.slave, command)
            .map_err(io::Error::other)?;
        let tree = job.into_guard(None);
        record_conpty_driver_stage("wait-descendant-pid")?;
        let leaf_pid_result = wait_for_pty_descendant_pid(
            &pid_file,
            &helper_failure_file,
            &helper_stage_file,
            child.as_mut(),
            operation_deadline,
        )
        .await;

        // 清理必须先于返回 PID 诊断；Job owner 的 drop 仍会兜底关闭残余成员。
        record_conpty_driver_stage("terminate-pty-job")?;
        let deadline = Instant::now() + Duration::from_secs(2);
        cleanup_deadline = Some(deadline);
        tree.terminate()?;
        record_conpty_driver_stage("wait-job-processes-exit")?;
        wait_for_pty_child_exit(child.as_mut(), deadline).await?;
        let leaf_pid = leaf_pid_result?;
        wait_for_process_exit(leaf_pid, deadline).await
    }
    .await;

    // child/Job 已结束后关闭两个 PTY 端点，在同一硬清理期限内确认 EOF 并 join reader。
    drop(pair);
    record_conpty_driver_stage("wait-output-eof")?;
    let cleanup_deadline =
        cleanup_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(2));
    let drain_completion = output_drain.finish_until(cleanup_deadline).await;
    let trace_result = record_conpty_driver_output_trace(&drain_completion.probe);
    let _ = fs::remove_file(pid_file);
    let _ = fs::remove_file(helper_failure_file);
    let _ = fs::remove_file(helper_stage_file);

    match (run_result, drain_completion.result.and(trace_result)) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(run_error), Ok(())) => Err(run_error),
        (Ok(()), Err(drain_error)) => Err(drain_error),
        (Err(run_error), Err(drain_error)) => Err(io::Error::other(format!(
            "{run_error}; ConPTY output-drain cleanup failed: {drain_error}"
        ))),
    }
}

/// 丢弃普通 ConPTY 输出、应答继承光标所需 DSR，并只记录请求/应答布尔值。
///
/// @author kongweiguang
#[cfg(windows)]
struct ConPtyOutputDrain {
    worker: Option<std::thread::JoinHandle<ConPtyOutputDrainCompletion>>,
}

/// 持有输出 drain 终态与不含原始 PTY 内容的 DSR 请求/应答摘要。
///
/// @author kongweiguang
#[cfg(windows)]
struct ConPtyOutputDrainCompletion {
    result: io::Result<()>,
    probe: ConPtyOutputProbe,
}

/// 只累计 DSR 请求/响应布尔值；保留最后三字节以识别跨 read 边界的查询。
///
/// @author kongweiguang
#[cfg(windows)]
#[derive(Default)]
struct ConPtyOutputProbe {
    saw_cursor_position_query: bool,
    replied_cursor_position_query: bool,
    cursor_tail: Vec<u8>,
}

#[cfg(windows)]
impl ConPtyOutputProbe {
    /// 仅识别 ConPTY 光标继承 DSR，绝不留存或打印 PTY 输出正文。
    ///
    /// @author kongweiguang
    fn observe(&mut self, bytes: &[u8]) -> usize {
        const CURSOR_QUERY: &[u8] = b"\x1b[6n";
        let mut cursor_window = Vec::with_capacity(self.cursor_tail.len() + bytes.len());
        cursor_window.extend_from_slice(&self.cursor_tail);
        cursor_window.extend_from_slice(bytes);
        let cursor_query_count = cursor_window
            .windows(CURSOR_QUERY.len())
            .filter(|window| *window == CURSOR_QUERY)
            .count();
        self.saw_cursor_position_query |= cursor_query_count > 0;
        let cursor_tail_start = cursor_window.len().saturating_sub(CURSOR_QUERY.len() - 1);
        self.cursor_tail = cursor_window[cursor_tail_start..].to_vec();
        cursor_query_count
    }

    /// 输出固定字段，确认 ConPTY 光标查询是否由测试宿主回应。
    ///
    /// @author kongweiguang
    fn summary(&self) -> String {
        format!(
            "cursor_dsr_seen={};cursor_dsr_replied={}",
            self.saw_cursor_position_query, self.replied_cursor_position_query,
        )
    }
}

#[cfg(windows)]
impl ConPtyOutputDrain {
    /// 用独立线程持续读取；只回答 ConPTY 明确启用继承光标标志所需的 DSR 请求。
    ///
    /// @author kongweiguang
    fn start(mut reader: Box<dyn io::Read + Send>, mut writer: Box<dyn io::Write + Send>) -> Self {
        let worker = std::thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            let mut probe = ConPtyOutputProbe::default();
            let mut result = Ok(());
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        let query_count = probe.observe(&buffer[..count]);
                        if query_count > 0 {
                            // portable-pty 的 INHERIT_CURSOR 只需标准 DSR 光标坐标响应；测试不验证 UI 坐标。
                            for _ in 0..query_count {
                                if let Err(error) = writer.write_all(b"\x1b[1;1R") {
                                    result = Err(error);
                                    break;
                                }
                            }
                            if result.is_ok()
                                && let Err(error) = writer.flush()
                            {
                                result = Err(error);
                            }
                            if result.is_err() {
                                break;
                            }
                            probe.replied_cursor_position_query = true;
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof
                        ) =>
                    {
                        break;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        result = Err(error);
                        break;
                    }
                }
            }
            ConPtyOutputDrainCompletion { result, probe }
        });
        Self {
            worker: Some(worker),
        }
    }

    /// 先等 ConPTY 关闭产生 EOF；超时作为失败返回，由外层 Job supervisor 回收仍阻塞的 driver reader。
    ///
    /// @author kongweiguang
    async fn finish_until(mut self, deadline: Instant) -> ConPtyOutputDrainCompletion {
        while self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
            && Instant::now() < deadline
        {
            sleep(Duration::from_millis(10)).await;
        }

        let Some(worker) = self.worker.take() else {
            return ConPtyOutputDrainCompletion {
                result: Err(io::Error::other("ConPTY output-drain worker is missing")),
                probe: ConPtyOutputProbe::default(),
            };
        };
        if !worker.is_finished() {
            // 超时必须失败；剩余同步读仅随隔离 driver 进程退出回收，不能把分离句柄报告为成功。
            drop(worker);
            return ConPtyOutputDrainCompletion {
                result: Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "ConPTY output drain did not reach EOF before its deadline",
                )),
                probe: ConPtyOutputProbe::default(),
            };
        }

        worker
            .join()
            .unwrap_or_else(|_| ConPtyOutputDrainCompletion {
                result: Err(io::Error::other("ConPTY output-drain thread panicked")),
                probe: ConPtyOutputProbe::default(),
            })
    }
}

/// 将不含正文的 DSR 请求/应答摘要写给隔离 supervisor，供 no-entry 超时判因。
///
/// @author kongweiguang
#[cfg(windows)]
fn record_conpty_driver_output_trace(probe: &ConPtyOutputProbe) -> io::Result<()> {
    let Some(path) = std::env::var_os(CONPTY_TEST_DRIVER_TRACE_ENV) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ConPTY test driver trace path is missing",
        ));
    };
    fs::write(path, probe.summary())
}

/// 在同一测试 deadline 内轮询直接 child；不让阻塞式 wait 越过 Job 清理预算。
///
/// @author kongweiguang
#[cfg(windows)]
async fn wait_for_pty_child_exit(
    child: &mut dyn portable_pty::Child,
    deadline: Instant,
) -> io::Result<()> {
    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "PTY helper did not exit before the Job cleanup deadline",
            ));
        }
        sleep(Duration::from_millis(10)).await;
    }
}

/// 等待 ConPTY helper 发布 PID；超时仅报告预定义阶段，避免从静默 PTY 猜测启动卡点。
///
/// @author kongweiguang
#[cfg(windows)]
async fn wait_for_pty_descendant_pid(
    pid_file: &std::path::Path,
    failure_file: &std::path::Path,
    stage_file: &std::path::Path,
    child: &mut dyn portable_pty::Child,
    operation_deadline: Instant,
) -> io::Result<u32> {
    loop {
        match fs::read_to_string(pid_file) {
            Ok(value) => {
                return value.trim().parse::<u32>().map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        match fs::read_to_string(failure_file) {
            Ok(diagnostic) => {
                let _ = fs::remove_file(failure_file);
                let stage = read_pty_helper_stage(stage_file);
                return Err(io::Error::other(format!(
                    "PTY helper failed before descendant PID publication (stage={stage}; {diagnostic})"
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        if let Some(status) = child.try_wait()? {
            let stage = read_pty_helper_stage(stage_file);
            return Err(io::Error::other(format!(
                "PTY helper exited before descendant PID publication (stage={stage}; success={}, exit_code={})",
                status.success(),
                status.exit_code()
            )));
        }

        if Instant::now() >= operation_deadline {
            let stage = read_pty_helper_stage(stage_file);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "PTY helper remained active without publishing descendant PID or failure details (stage={stage})"
                ),
            ));
        }
        sleep(Duration::from_millis(10)).await;
    }
}

/// 只接受 helper 写入的静态阶段名，避免把 PTY 正文或环境内容带进失败诊断。
///
/// @author kongweiguang
#[cfg(windows)]
fn read_pty_helper_stage(path: &std::path::Path) -> &'static str {
    match fs::read_to_string(path) {
        Ok(stage) => match stage.trim() {
            "root-entered" => "root-entered",
            "pid-path-read" => "pid-path-read",
            "program-resolved" => "program-resolved",
            "before-leaf-spawn" => "before-leaf-spawn",
            "leaf-created" => "leaf-created",
            "pid-published" => "pid-published",
            _ => "unknown",
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => "not-entered",
        Err(_) => "unavailable",
    }
}

/// 显式使用内嵌 process_tree 模块的类型路径，避免把生产子模块误当成测试 crate 根模块。
/// helper 始终从当前测试二进制启动，并使用独立临时 PID 文件验证真实进程树清理。
///
/// @author kongweiguang
fn spawn_root(kill_on_drop: bool) -> io::Result<(process_tree::spawn::ManagedTokioChild, PathBuf)> {
    let pid_file = temporary_pid_file();
    let executable = std::env::current_exe()?;
    let mut command = tokio::process::Command::new(executable);
    command
        .args([
            "--exact",
            "process_tree_spawn::process_tree_spawn_helper",
            "--nocapture",
        ])
        .env(process_helper::PROCESS_HELPER_ROLE, "root")
        .env(process_helper::PROCESS_HELPER_PID_FILE, &pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let child = ProcessTreeGuard::spawn_tokio(
        command,
        ProcessSpawnOptions {
            kill_on_drop,
            windows_creation_flags: 0,
        },
    )?;
    Ok((child, pid_file))
}

/// 轮询 helper 发布的 leaf PID；文件只位于本机临时目录且每次测试独立命名。
async fn wait_for_pid_file(path: &std::path::Path, deadline: Instant) -> io::Result<u32> {
    loop {
        match fs::read_to_string(path) {
            Ok(value) => {
                return value.trim().parse::<u32>().map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "process helper did not publish its descendant PID",
            ));
        }
        sleep(Duration::from_millis(10)).await;
    }
}

/// 等待 OS 确认 leaf 已退出，防止只凭 root wait 推断 Job/process group 收尾完成。
async fn wait_for_process_exit(pid: u32, deadline: Instant) -> io::Result<()> {
    loop {
        if !process_is_alive(pid)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "descendant process is still alive",
            ));
        }
        sleep(Duration::from_millis(10)).await;
    }
}

/// 用当前平台进程句柄确认指定 PID 是否仍运行，不读取命令行或系统进程列表。
fn process_is_alive(pid: u32) -> io::Result<bool> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::{
                CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, WAIT_OBJECT_0, WAIT_TIMEOUT,
            },
            System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
        };

        // SAFETY: OpenProcess receives a leaf PID emitted by this test and requests wait-only access.
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if process.is_null() {
            // SAFETY: GetLastError reads the thread-local error from the immediately preceding call.
            let error = unsafe { GetLastError() };
            if error == ERROR_INVALID_PARAMETER {
                return Ok(false);
            }
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        // SAFETY: the process handle was just opened with PROCESS_SYNCHRONIZE and is closed below.
        let wait = unsafe { WaitForSingleObject(process, 0) };
        // SAFETY: this test owns the handle returned by OpenProcess.
        unsafe { CloseHandle(process) };
        if wait == WAIT_TIMEOUT {
            Ok(true)
        } else if wait == WAIT_OBJECT_0 {
            Ok(false)
        } else {
            // SAFETY: GetLastError reports an unexpected wait result from this thread.
            Err(io::Error::from_raw_os_error(
                unsafe { GetLastError() } as i32
            ))
        }
    }
    #[cfg(unix)]
    {
        let pid = libc::pid_t::try_from(pid)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "PID does not fit pid_t"))?;
        // SAFETY: signal 0 only probes the test-owned process and does not deliver a signal.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(false)
        } else {
            Err(error)
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process liveness probe is unsupported on this platform",
        ))
    }
}

/// 为多个并行 test process 创建互不重叠的 leaf PID 临时文件。
fn temporary_pid_file() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "kerminal-process-tree-{}-{nonce}.pid",
        std::process::id()
    ))
}
