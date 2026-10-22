//! 使用真实本地 PTY 子进程覆盖关闭期间的输出、背压和资源回收。
//!
//! @author kongweiguang

use std::collections::BTreeMap;
#[cfg(windows)]
use std::ffi::OsStr;
use std::ffi::OsString;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use kerminal_application::sessions::{SessionTransportControl, SessionTransportFactory};
use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::{
    LocalSessionTarget, SessionExitStatus, SessionTarget, TerminalSize,
};
use kerminal_infrastructure::local_pty::LocalPtySessionTransportFactory;
use kerminal_terminal::{TerminalEngine, TerminalLimits};
use tempfile::TempDir;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep, timeout_at};
use tokio_util::sync::CancellationToken;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, GetLastError, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

const TEST_DEADLINE: Duration = Duration::from_secs(180);
const CLOSE_DEADLINE: Duration = Duration::from_secs(12);
const ACTIVE_OUTPUT_THRESHOLD: u64 = 64 * 1024;
const OUTPUT_SAMPLE_LIMIT: usize = 16 * 1024;
const TERMINAL_COLUMNS: u16 = 100;
const TERMINAL_ROWS: u16 = 30;
const TERMINAL_READY_MARKER: &str = "KERMINAL_TERMINAL_READY";
const STDOUT_LOAD_MARKER: &str = "KERMINAL_STDOUT_LOAD";
const STDERR_LOAD_MARKER: &str = "KERMINAL_STDERR_LOAD";
const OBSERVED_WINDOWS_DSR_QUERY: &[u8] = b"\x1b[6n";
const PARSER_REPLY_DEADLINE: Duration = Duration::from_secs(2);
const BACKPRESSURE_STABILITY_WINDOW: Duration = Duration::from_millis(100);
const CONCURRENT_CLOSE_COUNT: usize = 8;
const CREATE_CLOSE_CYCLE_COUNT: usize = 100;

static TEST_STARTED_AT: OnceLock<Instant> = OnceLock::new();

/// 在每个可能阻塞的异步阶段前后输出无敏感内容的时间线，便于外部 watchdog 定位最后卡住的调用。
///
/// @author kongweiguang
fn stage_diagnostic(
    stage: &str,
    event: &str,
    process_id: Option<u32>,
    details: impl std::fmt::Display,
) {
    let elapsed_ms = TEST_STARTED_AT
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis();
    eprintln!(
        "[local-pty-acceptance] elapsed_ms={elapsed_ms} stage={stage} event={event} pid={process_id:?} details={details}"
    );
}

/// 为子进程准备临时配置根，避免读取或修改用户的 Kerminal 数据。
///
/// @author kongweiguang
struct IsolatedConfigRoot {
    _directory: TempDir,
    config_root: PathBuf,
}

impl IsolatedConfigRoot {
    /// 保留临时根目录所有权，测试结束后由 TempDir 清理本测试创建的数据。
    ///
    /// @author kongweiguang
    fn create() -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("kerminal-local-pty-close-")
            .tempdir()
            .context("创建 PTY 测试专用目录")?;
        let config_root = directory.path().join("config");
        std::fs::create_dir_all(&config_root).context("创建 PTY 测试配置根")?;
        Ok(Self {
            _directory: directory,
            config_root,
        })
    }

    /// 返回注入子进程并用于失败诊断的临时根目录，不会读取其中的用户配置。
    ///
    /// @author kongweiguang
    fn path(&self) -> &std::path::Path {
        &self.config_root
    }
}

/// 只把测试私有配置根注入自己创建的 shell，避免修改 Tokio 进程中其他线程可见的环境。
///
/// @author kongweiguang
fn isolated_child_environment(config_root: &Path) -> BTreeMap<OsString, OsString> {
    BTreeMap::from([(
        OsString::from("KERMINAL_CONFIG_ROOT"),
        config_root.as_os_str().to_owned(),
    )])
}

/// 固定测试子进程身份，避免关闭后再次按 PID 查询时把复用该 PID 的新进程误当成原进程。
///
/// Windows 在 PTY 创建后立即打开 waitable process handle，并一直持有到退出确认；
/// 权限不足作为探针错误上报，只有系统明确表示该 PID 已不存在时才按已退出处理。
///
/// @author kongweiguang
struct ProcessExitProbe {
    process_id: u32,
    #[cfg(windows)]
    handle: Option<OwnedHandle>,
}

impl ProcessExitProbe {
    /// 在仍持有本测试 PTY 的时机获取稳定进程句柄；Unix 保留既有 PID 探针。
    ///
    /// @author kongweiguang
    fn capture(process_id: u32) -> Result<Self> {
        ensure!(process_id != 0, "PTY factory 返回了无效 PID 0");

        #[cfg(windows)]
        {
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id) };
            if handle.is_null() {
                let error = unsafe { GetLastError() };
                if error == ERROR_INVALID_PARAMETER {
                    return Ok(Self {
                        process_id,
                        handle: None,
                    });
                }
                return Err(anyhow!(
                    "为测试 PTY 打开进程等待句柄失败，pid={process_id}, win32_error={error}"
                ));
            }
            // SAFETY: OpenProcess 返回一个新拥有的 HANDLE；OwnedHandle 接管后恰好关闭一次。
            let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
            Ok(Self {
                process_id,
                handle: Some(handle),
            })
        }

        #[cfg(unix)]
        Ok(Self { process_id })
    }

    /// 用固定进程对象的非阻塞 wait 查询存活状态，不重新解析 PID。
    ///
    /// @author kongweiguang
    fn is_running(&self) -> Result<bool> {
        #[cfg(windows)]
        {
            let Some(handle) = self.handle.as_ref() else {
                return Ok(false);
            };
            let result = unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) };
            match result {
                WAIT_OBJECT_0 => Ok(false),
                WAIT_TIMEOUT => Ok(true),
                WAIT_FAILED => {
                    let error = unsafe { GetLastError() };
                    Err(anyhow!(
                        "等待测试 PTY 进程句柄失败，pid={}, win32_error={error}",
                        self.process_id
                    ))
                }
                other => Err(anyhow!(
                    "测试 PTY 进程句柄返回未知 wait 状态，pid={}, status={other}",
                    self.process_id
                )),
            }
        }

        #[cfg(unix)]
        process_is_running(self.process_id)
    }
}

/// 只持有本测试创建的 PTY 控制句柄和流端点；每个场景在返回前显式 await 清理。
///
/// @author kongweiguang
struct OwnedPty {
    label: &'static str,
    process_id: u32,
    process_probe: Option<ProcessExitProbe>,
    control: Arc<dyn SessionTransportControl>,
    reader: Option<Pin<Box<dyn AsyncRead + Send + 'static>>>,
    writer: Option<Pin<Box<dyn AsyncWrite + Send + 'static>>>,
    close_started: Arc<AtomicBool>,
    closed: bool,
}

impl OwnedPty {
    /// 返回仅属于当前 PTY 的进程身份探针，避免关闭后退化为 PID 重查。
    ///
    /// @author kongweiguang
    fn process_probe(&self) -> Result<&ProcessExitProbe> {
        self.process_probe.as_ref().ok_or_else(|| {
            anyhow!(
                "{} 的进程身份探针已被移交，pid={}",
                self.label,
                self.process_id
            )
        })
    }

    /// 将已关闭 PTY 的进程身份句柄移交给后续退出验证并继续持有到 wait 完成。
    ///
    /// @author kongweiguang
    fn take_process_probe(&mut self) -> Result<ProcessExitProbe> {
        self.process_probe.take().ok_or_else(|| {
            anyhow!(
                "{} 的进程身份探针已被移交，pid={}",
                self.label,
                self.process_id
            )
        })
    }

    /// 交出唯一 reader 所有权供输出消费任务使用，控制句柄仍由本对象负责清理。
    ///
    /// @author kongweiguang
    fn take_reader(&mut self) -> Result<Pin<Box<dyn AsyncRead + Send + 'static>>> {
        self.reader.take().ok_or_else(|| {
            anyhow!(
                "{} 的 PTY reader 已被取走，pid={}",
                self.label,
                self.process_id
            )
        })
    }

    /// 先终止进程树，再释放 duplex 消费端，最后等待所有 PTY worker 在同一 deadline 内退出。
    ///
    /// 此顺序让 reader worker 能区分关闭引起的 BrokenPipe，并在消费端背压时解除有界桥接阻塞。
    ///
    /// @author kongweiguang
    async fn close_until(&mut self, deadline: Instant) -> Result<SessionExitStatus> {
        self.close_started.store(true, Ordering::Release);
        stage_diagnostic(self.label, "close.begin", Some(self.process_id), "request");
        let close_result = match timeout_at(deadline, self.control.close_until(deadline)).await {
            Ok(result) => result.map_err(|error| anyhow!("{error}")),
            Err(_) => Err(anyhow!(
                "{} close 超时，pid={}",
                self.label,
                self.process_id
            )),
        };
        stage_diagnostic(
            self.label,
            "close.end",
            Some(self.process_id),
            if close_result.is_ok() { "ok" } else { "error" },
        );

        // 关闭进程树后再丢弃 duplex 端点；这样关闭标记已发布，桥接 BrokenPipe 属于预期收尾。
        drop(self.reader.take());
        drop(self.writer.take());

        stage_diagnostic(
            self.label,
            "wait_exit.begin",
            Some(self.process_id),
            "request",
        );
        let exit_result = match timeout_at(deadline, self.control.wait_exit()).await {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(error)) => Err(anyhow!("{error}")),
            Err(_) => Err(anyhow!(
                "{} wait_exit 超时，pid={}",
                self.label,
                self.process_id
            )),
        };
        stage_diagnostic(
            self.label,
            "wait_exit.end",
            Some(self.process_id),
            if exit_result.is_ok() { "ok" } else { "error" },
        );

        match (close_result, exit_result) {
            (Ok(()), Ok(status)) => {
                self.closed = true;
                Ok(status)
            }
            (Err(close_error), Ok(_)) => Err(close_error.context(format!(
                "{} 关闭失败但退出状态已回收，pid={}",
                self.label, self.process_id
            ))),
            (Ok(()), Err(exit_error)) => Err(exit_error.context(format!(
                "{} 已关闭但 worker 未完成回收，pid={}",
                self.label, self.process_id
            ))),
            (Err(close_error), Err(exit_error)) => Err(anyhow!(
                "{} close 与 wait_exit 均失败，pid={}；close={close_error:#}；wait_exit={exit_error:#}",
                self.label,
                self.process_id
            )),
        }
    }
}

impl Drop for OwnedPty {
    /// 在显式异步清理未完成时留下可检索诊断，不把后台任务排队误报成资源已回收。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        eprintln!(
            "local PTY cleanup was not confirmed before owner drop: label={}, pid={}",
            self.label, self.process_id
        );
    }
}

/// 失败路径继续使用同一控制句柄进行有界回收，并保留原始断言错误作为主因。
///
/// @author kongweiguang
async fn return_after_owned_cleanup<T>(probe: &mut OwnedPty, error: anyhow::Error) -> Result<T> {
    if probe.closed {
        return Err(error);
    }
    stage_diagnostic(
        probe.label,
        "failure_cleanup.begin",
        Some(probe.process_id),
        "bounded retry",
    );
    match probe.close_until(Instant::now() + CLOSE_DEADLINE).await {
        Ok(_) => {
            stage_diagnostic(
                probe.label,
                "failure_cleanup.end",
                Some(probe.process_id),
                "closed",
            );
            Err(error.context("场景失败；测试自建 PTY 已完成后续清理"))
        }
        Err(cleanup_error) => {
            stage_diagnostic(
                probe.label,
                "failure_cleanup.end",
                Some(probe.process_id),
                "unconfirmed",
            );
            Err(error.context(format!(
                "场景失败且测试自建 PTY 后续清理未确认: {cleanup_error:#}"
            )))
        }
    }
}

/// 在并发场景尚未全部创建成功时逐个 await 已拥有的 PTY 清理，避免早退留下子进程。
///
/// @author kongweiguang
async fn close_all_owned(probes: &mut [OwnedPty]) -> Vec<String> {
    let mut failures = Vec::new();
    for probe in probes {
        if let Err(first_error) = probe.close_until(Instant::now() + CLOSE_DEADLINE).await
            && let Err(recovery_error) = probe.close_until(Instant::now() + CLOSE_DEADLINE).await
        {
            failures.push(format!(
                "pid={}: first={first_error:#}; recovery={recovery_error:#}",
                probe.process_id
            ));
        }
    }
    failures
}

/// 把单阶段操作限制在总测试绝对 deadline 内，同时保留独立清理 deadline。
///
/// @author kongweiguang
fn stage_deadline(total_deadline: Instant, maximum: Duration) -> Instant {
    (Instant::now() + maximum).min(total_deadline)
}

/// 在绝对 deadline 内等待 consumer；超时后最多再等两秒确认取消，避免 harness 自己无界 join。
///
/// @author kongweiguang
async fn join_output_consumer(
    consumer: &mut tokio::task::JoinHandle<Result<OutputStats>>,
    deadline: Instant,
    label: &str,
    process_id: u32,
) -> Result<OutputStats> {
    stage_diagnostic(label, "consumer_join.begin", Some(process_id), "normal");
    match timeout_at(deadline, &mut *consumer).await {
        Ok(joined) => {
            stage_diagnostic(label, "consumer_join.end", Some(process_id), "completed");
            joined
                .context("输出 consumer task 被取消")?
                .context("消费本地 PTY 输出失败")
        }
        Err(_) => {
            stage_diagnostic(
                label,
                "consumer_abort.begin",
                Some(process_id),
                "deadline elapsed",
            );
            consumer.abort();
            let join_deadline = Instant::now() + Duration::from_secs(2);
            let abort_join = timeout_at(join_deadline, &mut *consumer).await;
            stage_diagnostic(
                label,
                "consumer_abort.end",
                Some(process_id),
                if abort_join.is_ok() {
                    "joined within 2s"
                } else {
                    "join still pending after 2s"
                },
            );
            Err(anyhow!(
                "等待输出 consumer task 超过绝对 deadline；abort_join={}",
                if abort_join.is_ok() {
                    "completed"
                } else {
                    "pending"
                }
            ))
        }
    }
}

/// 累计真实 PTY 消费字节，记录 parser 回写数，并保留有界原始样本用于失败定位。
///
/// @author kongweiguang
#[derive(Debug)]
struct OutputStats {
    bytes: u64,
    sample: Vec<u8>,
    parser_replies: usize,
}

/// 保存背压场景握手期间已经被真实终端解析器消费的字节，避免与关闭后 drain 混算。
///
/// @author kongweiguang
#[derive(Debug)]
struct HandshakeStats {
    bytes: u64,
    sample: Vec<u8>,
    parser_replies: usize,
}

/// 在原始 PTY 字节流中识别测试 shell 标记，避免网格软换行把字面量拆成多行。
///
/// @author kongweiguang
#[derive(Default)]
struct PtyOutputMarkerTracker {
    carry: Vec<u8>,
    terminal_ready_seen: bool,
    stdout_seen: bool,
    stderr_seen: bool,
    current_o_run: usize,
    current_e_run: usize,
    longest_o_run: usize,
    longest_e_run: usize,
}

impl PtyOutputMarkerTracker {
    /// 跨 read chunk 搜索真实标记，并只累计有限长度的 O/E 连续字节诊断，不保留无限 raw 输出。
    ///
    /// @author kongweiguang
    fn observe(&mut self, chunk: &[u8]) {
        for byte in chunk {
            match byte {
                b'O' => {
                    self.current_o_run = self.current_o_run.saturating_add(1);
                    self.current_e_run = 0;
                    self.longest_o_run = self.longest_o_run.max(self.current_o_run);
                }
                b'E' => {
                    self.current_e_run = self.current_e_run.saturating_add(1);
                    self.current_o_run = 0;
                    self.longest_e_run = self.longest_e_run.max(self.current_e_run);
                }
                _ => {
                    self.current_o_run = 0;
                    self.current_e_run = 0;
                }
            }
        }
        if self.all_seen() {
            return;
        }
        let mut candidate = Vec::with_capacity(self.carry.len().saturating_add(chunk.len()));
        candidate.extend_from_slice(&self.carry);
        candidate.extend_from_slice(chunk);

        self.terminal_ready_seen |=
            contains_literal_bytes(&candidate, TERMINAL_READY_MARKER.as_bytes());
        self.stdout_seen |= contains_literal_bytes(&candidate, STDOUT_LOAD_MARKER.as_bytes());
        self.stderr_seen |= contains_literal_bytes(&candidate, STDERR_LOAD_MARKER.as_bytes());

        let carry_limit = TERMINAL_READY_MARKER
            .len()
            .max(STDOUT_LOAD_MARKER.len())
            .max(STDERR_LOAD_MARKER.len())
            .saturating_sub(1);
        let carry_length = candidate.len().min(carry_limit);
        self.carry.clear();
        self.carry
            .extend_from_slice(&candidate[candidate.len() - carry_length..]);
    }

    /// 只有三种 marker 都从 PTY 原始输出中出现后才允许进入背压阶段。
    ///
    /// @author kongweiguang
    fn all_seen(&self) -> bool {
        self.terminal_ready_seen && self.stdout_seen && self.stderr_seen
    }
}

/// 搜索非空 ASCII marker；输入 chunk 可能带有任意终端控制字节，但不改写其原始内容。
///
/// @author kongweiguang
fn contains_literal_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// 绑定背压测试的真实 shell target 与其私有 producer 进度文件。
///
/// @author kongweiguang
struct BackpressureTarget {
    target: LocalSessionTarget,
    progress_path: PathBuf,
    start_path: PathBuf,
}

/// 复用与 PTY 完全相同网格尺寸创建终端仿真器，确保压力测试解析真实终端协议。
///
/// @author kongweiguang
fn create_terminal_engine() -> Result<TerminalEngine> {
    let size = terminal_size()?;
    TerminalEngine::new(SessionId::new(), size, TerminalLimits::default())
        .context("创建 PTY 压力测试终端解析器")
}

/// 统一网格来源，避免终端 parser 与真实 PTY 的列行数发生偏差。
///
/// @author kongweiguang
fn terminal_size() -> Result<TerminalSize> {
    TerminalSize::new(TERMINAL_COLUMNS, TERMINAL_ROWS, 0, 0).context("创建压力测试终端尺寸")
}

/// 创建平台对应的真实终端命令；先写就绪标记，再持续写入双流输出。
///
/// @author kongweiguang
fn flood_target(config_root: &Path) -> LocalSessionTarget {
    #[cfg(windows)]
    let (program, args) = (
        "powershell.exe",
        vec![
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Console]::Out.WriteLine('KERMINAL_TERMINAL_READY'); while ($true) { [Console]::Out.WriteLine('KERMINAL_STDOUT_LOAD'); [Console]::Error.WriteLine('KERMINAL_STDERR_LOAD') }",
        ],
    );
    #[cfg(unix)]
    let (program, args) = (
        "/bin/sh",
        vec![
            "-c",
            "printf 'KERMINAL_TERMINAL_READY\\n'; while :; do printf 'KERMINAL_STDOUT_LOAD\\n'; printf 'KERMINAL_STDERR_LOAD\\n' >&2; done",
        ],
    );

    LocalSessionTarget {
        program: OsString::from(program),
        args: args.into_iter().map(OsString::from).collect(),
        working_directory: None,
        environment: isolated_child_environment(config_root),
    }
}

/// 创建先等待外部 gate 的真实双流 shell，避免握手读取阶段提前耗尽 PTY 输出队列。
///
/// 进度与 gate 都位于本测试临时根；每路 payload 后换行，使下一路 marker 从新行开始。
///
/// @author kongweiguang
fn backpressure_target(config_root: &Path) -> Result<BackpressureTarget> {
    let progress_path = config_root.join("pty-backpressure-progress.txt");
    let start_path = config_root.join("pty-backpressure-start.signal");

    #[cfg(windows)]
    let target = {
        let command = r#"
$progressPath = $env:KERMINAL_TEST_PROGRESS_PATH
$startPath = $env:KERMINAL_TEST_START_PATH
[System.IO.File]::WriteAllText($progressPath, '0')
[Console]::Out.WriteLine('KERMINAL_TERMINAL_READY')
[Console]::Out.WriteLine('KERMINAL_STDOUT_LOAD')
[Console]::Out.Flush()
[Console]::Error.WriteLine('KERMINAL_STDERR_LOAD')
[Console]::Error.Flush()
while (-not [System.IO.File]::Exists($startPath)) { Start-Sleep -Milliseconds 5 }
$payloadOut = ('O' * 4096) -join ''
$payloadErr = ('E' * 4096) -join ''
$count = 0
while ($true) {
    [Console]::Out.Write($payloadOut)
    [Console]::Out.WriteLine()
    [Console]::Out.Flush()
    $count += 4096
    [System.IO.File]::WriteAllText($progressPath, [string]$count)
    [Console]::Error.Write($payloadErr)
    [Console]::Error.WriteLine()
    [Console]::Error.Flush()
    $count += 4096
    [System.IO.File]::WriteAllText($progressPath, [string]$count)
}
"#;
        let program = backpressure_test_shell_program()?;
        let mut environment = isolated_child_environment(config_root);
        environment.insert(
            OsString::from("KERMINAL_TEST_PROGRESS_PATH"),
            progress_path.as_os_str().to_owned(),
        );
        environment.insert(
            OsString::from("KERMINAL_TEST_START_PATH"),
            start_path.as_os_str().to_owned(),
        );
        LocalSessionTarget {
            program,
            args: [
                OsString::from("-NoLogo"),
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-Command"),
                OsString::from(command),
            ]
            .into_iter()
            .collect(),
            working_directory: None,
            environment,
        }
    };
    #[cfg(unix)]
    let target = LocalSessionTarget {
        program: OsString::from("/bin/sh"),
        args: [
            OsString::from("-c"),
            OsString::from(
                "progress_path=$1; start_path=$2; printf '0' > \"$progress_path\"; printf 'KERMINAL_TERMINAL_READY\\n'; printf 'KERMINAL_STDOUT_LOAD\\n'; printf 'KERMINAL_STDERR_LOAD\\n' >&2; while [ ! -e \"$start_path\" ]; do sleep 0.01; done; count=0; payload_out=$(printf '%4096s' '' | tr ' ' O); payload_err=$(printf '%4096s' '' | tr ' ' E); while :; do printf '%s\\n' \"$payload_out\"; count=$((count + 4096)); printf '%s' \"$count\" > \"$progress_path\"; printf '%s\\n' \"$payload_err\" >&2; count=$((count + 4096)); printf '%s' \"$count\" > \"$progress_path\"; done",
            ),
            OsString::from("kerminal-pty-backpressure"),
            progress_path.as_os_str().to_owned(),
            start_path.as_os_str().to_owned(),
        ]
        .into_iter()
        .collect(),
        working_directory: None,
        environment: isolated_child_environment(config_root),
    };

    Ok(BackpressureTarget {
        target,
        progress_path,
        start_path,
    })
}

/// 仅允许默认 PowerShell 5.1 或经过验证的绝对 exe 路径，避免 pwsh App Execution Alias 被当作 shell。
///
/// 裸 `pwsh` 会从 PATH 解析，无法证明启动的是实际运行时二进制，因此测试矩阵要求使用绝对路径。
///
/// @author kongweiguang
#[cfg(windows)]
fn backpressure_test_shell_program() -> Result<OsString> {
    let Some(selection) = std::env::var_os("KERMINAL_PTY_TEST_SHELL") else {
        return Ok(OsString::from("powershell.exe"));
    };

    if selection.as_os_str() == OsStr::new("powershell")
        || selection.as_os_str() == OsStr::new("powershell.exe")
    {
        return Ok(OsString::from("powershell.exe"));
    }
    if selection.as_os_str() == OsStr::new("pwsh")
        || selection.as_os_str() == OsStr::new("pwsh.exe")
    {
        return Err(anyhow!(
            "KERMINAL_PTY_TEST_SHELL 的 pwsh 选择必须使用真实 pwsh.exe 的绝对路径"
        ));
    }

    let executable = PathBuf::from(selection);
    ensure!(
        executable.is_absolute(),
        "KERMINAL_PTY_TEST_SHELL 必须是 powershell.exe（默认）或 powershell.exe/pwsh.exe 的绝对路径"
    );
    ensure!(
        executable.is_file(),
        "KERMINAL_PTY_TEST_SHELL 指定的可执行文件不存在"
    );
    let file_name = executable
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| anyhow!("KERMINAL_PTY_TEST_SHELL 路径文件名无效"))?;
    ensure!(
        file_name.eq_ignore_ascii_case("powershell.exe")
            || file_name.eq_ignore_ascii_case("pwsh.exe"),
        "KERMINAL_PTY_TEST_SHELL 绝对路径的文件名必须是 powershell.exe 或 pwsh.exe"
    );
    Ok(executable.into_os_string())
}

/// 创建无用户配置依赖的长时子进程，供并发关闭与百轮生命周期回归使用。
///
/// @author kongweiguang
fn idle_target(config_root: &Path) -> LocalSessionTarget {
    #[cfg(windows)]
    let (program, args) = (
        "powershell.exe",
        vec![
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep -Seconds 120",
        ],
    );
    #[cfg(unix)]
    let (program, args) = ("/bin/sh", vec!["-c", "exec sleep 120"]);

    LocalSessionTarget {
        program: OsString::from(program),
        args: args.into_iter().map(OsString::from).collect(),
        working_directory: None,
        environment: isolated_child_environment(config_root),
    }
}

/// 通过生产 factory 打开真实 PTY；打开失败携带 cleanup owner 时也在 deadline 内回收。
///
/// @author kongweiguang
async fn open_owned_pty(
    factory: &LocalPtySessionTransportFactory,
    label: &'static str,
    target: LocalSessionTarget,
) -> Result<OwnedPty> {
    let size = terminal_size()?;
    stage_diagnostic(label, "open.begin", None, "factory.open");
    let transport = match factory
        .open(
            &SessionTarget::Local(target),
            size,
            CancellationToken::new(),
        )
        .await
    {
        Ok(transport) => {
            stage_diagnostic(label, "open.end", transport.process_id, "ok");
            transport
        }
        Err(error) => {
            stage_diagnostic(label, "open.end", None, "error");
            let (primary, cleanup) = error.into_parts();
            let mut cleanup_failure = None;
            if let Some(cleanup) = cleanup {
                let deadline = Instant::now() + CLOSE_DEADLINE;
                let first = timeout_at(deadline, cleanup.close_until(deadline)).await;
                if !matches!(&first, Ok(Ok(()))) {
                    let recovery_deadline = Instant::now() + CLOSE_DEADLINE;
                    let recovery =
                        timeout_at(recovery_deadline, cleanup.close_until(recovery_deadline)).await;
                    if !matches!(&recovery, Ok(Ok(()))) {
                        cleanup_failure = Some(format!(
                            "{label} open cleanup 未回收: first={first:?}; recovery={recovery:?}"
                        ));
                    }
                }
            }
            let primary: anyhow::Error = primary.into();
            let primary = primary.context(format!("打开 {label} PTY"));
            return Err(match cleanup_failure {
                Some(cleanup_error) => primary.context(cleanup_error),
                None => primary,
            });
        }
    };
    let Some(process_id) = transport.process_id else {
        let (reader, writer, control) = (transport.reader, transport.writer, transport.control);
        let deadline = Instant::now() + CLOSE_DEADLINE;
        let first_close = timeout_at(deadline, control.close_until(deadline)).await;
        drop(reader);
        drop(writer);
        let first_exit = timeout_at(deadline, control.wait_exit()).await;
        let (recovery_close, recovery_exit) = if matches!(&first_close, Ok(Ok(())))
            && matches!(&first_exit, Ok(Ok(_)))
        {
            (None, None)
        } else {
            let recovery_deadline = Instant::now() + CLOSE_DEADLINE;
            let close = timeout_at(recovery_deadline, control.close_until(recovery_deadline)).await;
            let exit = timeout_at(recovery_deadline, control.wait_exit()).await;
            (Some(close), Some(exit))
        };
        let recovered = recovery_close
            .as_ref()
            .is_some_and(|result| matches!(result, Ok(Ok(()))))
            && recovery_exit
                .as_ref()
                .is_some_and(|result| matches!(result, Ok(Ok(_))));
        ensure!(
            (matches!(&first_close, Ok(Ok(()))) && matches!(&first_exit, Ok(Ok(_)))) || recovered,
            "{label} PTY 未提供 pid 且资源清理失败: close={first_close:?}, wait_exit={first_exit:?}, recovery_close={recovery_close:?}, recovery_exit={recovery_exit:?}"
        );
        return Err(anyhow!("{label} PTY 未暴露本地 child pid"));
    };
    let mut probe = OwnedPty {
        label,
        process_id,
        process_probe: None,
        control: Arc::clone(&transport.control),
        reader: Some(transport.reader),
        writer: Some(transport.writer),
        close_started: Arc::new(AtomicBool::new(false)),
        closed: false,
    };
    match ProcessExitProbe::capture(process_id) {
        Ok(process_probe) => {
            probe.process_probe = Some(process_probe);
            Ok(probe)
        }
        Err(error) => {
            return_after_owned_cleanup(&mut probe, error.context("捕获测试 PTY 的稳定进程身份句柄"))
                .await
        }
    }
}

/// 持续解析真实 PTY 字节并回写 parser 回复，同时保留有界原始输出样本。
///
/// @author kongweiguang
async fn consume_terminal_output(
    mut reader: Pin<Box<dyn AsyncRead + Send + 'static>>,
    mut writer: Pin<Box<dyn AsyncWrite + Send + 'static>>,
    byte_count: Arc<AtomicU64>,
    close_started: Arc<AtomicBool>,
    mut engine: TerminalEngine,
    label: &'static str,
    process_id: u32,
) -> Result<OutputStats> {
    stage_diagnostic(
        label,
        "reader.begin",
        Some(process_id),
        "consume PTY output",
    );
    let mut buffer = [0_u8; 16 * 1024];
    let mut bytes = 0_u64;
    let mut sample = Vec::with_capacity(OUTPUT_SAMPLE_LIMIT);
    let mut parser_replies = 0_usize;
    let mut first_read = true;
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(read) => read,
            Err(error) => {
                stage_diagnostic(
                    label,
                    "reader.error",
                    Some(process_id),
                    format!("bytes={bytes}"),
                );
                return Err(error).context("读取本地 PTY 输出");
            }
        };
        if first_read {
            first_read = false;
            stage_diagnostic(
                label,
                "reader.first_read.end",
                Some(process_id),
                format!("bytes={read}"),
            );
        }
        if read == 0 {
            break;
        }

        let output = engine.process_output(&buffer[..read]);
        for response in output.pty_writes {
            let write_result = match timeout_at(
                Instant::now() + PARSER_REPLY_DEADLINE,
                writer.as_mut().write_all(&response),
            )
            .await
            {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(anyhow!(error).context("写回终端 parser 协议回复")),
                Err(_) => Err(anyhow!("写回终端 parser 协议回复超时")),
            };
            if let Err(error) = write_result {
                if close_started.load(Ordering::Acquire) {
                    break;
                }
                return Err(error);
            }
            parser_replies = parser_replies.saturating_add(1);
        }

        bytes = bytes.saturating_add(read as u64);
        byte_count.fetch_add(read as u64, Ordering::Release);
        let remaining = OUTPUT_SAMPLE_LIMIT.saturating_sub(sample.len());
        sample.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    stage_diagnostic(
        label,
        "reader.end",
        Some(process_id),
        format!("bytes={bytes} parser_replies={parser_replies}"),
    );
    Ok(OutputStats {
        bytes,
        sample,
        parser_replies,
    })
}

/// 等待 consumer 真正收到指定输出量；超时时记录已读计数而不把 reader 队列长度当成消费量。
///
/// @author kongweiguang
async fn wait_for_output_bytes(counter: &AtomicU64, minimum: u64, deadline: Instant) -> Result<()> {
    match timeout_at(deadline, async {
        while counter.load(Ordering::Acquire) < minimum {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    {
        Ok(()) => Ok(()),
        Err(_) => Err(anyhow!(
            "PTY 输出在 deadline 前未达到 {minimum} 字节；consumer_actual_bytes={}",
            counter.load(Ordering::Acquire)
        )),
    }
}

/// 对合成 flood 的首段输出同时生成转义文本与十六进制，定位原始 PTY 启动握手。
///
/// @author kongweiguang
fn format_output_prefix(sample: &[u8]) -> (String, String) {
    let prefix = &sample[..sample.len().min(128)];
    let escaped = String::from_utf8_lossy(prefix).escape_debug().to_string();
    let hexadecimal = prefix
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    (escaped, hexadecimal)
}

/// 仅识别失败日志中真实观察到的 DSR 请求；回复内容仍完全由 TerminalEngine 生成。
///
/// @author kongweiguang
fn observed_device_status_query(sample: &[u8]) -> bool {
    sample
        .windows(OBSERVED_WINDOWS_DSR_QUERY.len())
        .any(|window| window == OBSERVED_WINDOWS_DSR_QUERY)
}

/// 消费启动握手并从 PTY 原始字节确认三个 marker，避免把初始 flood 误算成受控压力阶段。
///
/// @author kongweiguang
async fn answer_terminal_queries_until_ready(
    reader: &mut Pin<Box<dyn AsyncRead + Send + 'static>>,
    writer: &mut Pin<Box<dyn AsyncWrite + Send + 'static>>,
    engine: &mut TerminalEngine,
    deadline: Instant,
) -> Result<HandshakeStats> {
    let mut buffer = [0_u8; 16 * 1024];
    let mut bytes = 0_u64;
    let mut sample = Vec::with_capacity(OUTPUT_SAMPLE_LIMIT);
    let mut parser_replies = 0_usize;
    let mut markers = PtyOutputMarkerTracker::default();
    let mut iteration = 0_usize;

    loop {
        // Tokio timeout 会先 poll 内层 future；连续立即就绪的 read 可能在同一 task poll 中越过 deadline。
        if Instant::now() >= deadline {
            stage_diagnostic(
                "backpressure-handshake",
                "deadline.expired",
                None,
                format!("point=before-read iteration={iteration} bytes={bytes}"),
            );
            return Err(handshake_deadline_error(
                "before-read",
                iteration,
                bytes,
                parser_replies,
                &markers,
                &sample,
            ));
        }

        // 只记录前八轮和后续 2 的幂轮次，避免 flood 日志改变背压场景。
        let trace_iteration = iteration < 8 || iteration.is_power_of_two();
        if trace_iteration {
            stage_diagnostic(
                "backpressure-handshake",
                "read.begin",
                None,
                format!("iteration={iteration} bytes={bytes}"),
            );
        }
        let read_result = timeout_at(deadline, reader.as_mut().read(&mut buffer)).await;
        let read = match read_result {
            Ok(Ok(read)) => {
                if trace_iteration {
                    stage_diagnostic(
                        "backpressure-handshake",
                        "read.end",
                        None,
                        format!("iteration={iteration} bytes={read}"),
                    );
                }
                read
            }
            Ok(Err(error)) => {
                if trace_iteration {
                    stage_diagnostic(
                        "backpressure-handshake",
                        "read.end",
                        None,
                        format!("iteration={iteration} result=error kind={:?}", error.kind()),
                    );
                }
                return Err(error).context("读取 PTY 启动握手输出");
            }
            Err(_) => {
                stage_diagnostic(
                    "backpressure-handshake",
                    "read.end",
                    None,
                    format!("iteration={iteration} result=timeout"),
                );
                return Err(handshake_deadline_error(
                    "reader-read-pending",
                    iteration,
                    bytes,
                    parser_replies,
                    &markers,
                    &sample,
                ));
            }
        };
        ensure!(
            read > 0,
            "PTY 在终端就绪标记前关闭；bytes={bytes}, parser_replies={parser_replies}"
        );
        markers.observe(&buffer[..read]);

        if trace_iteration {
            stage_diagnostic(
                "backpressure-handshake",
                "process_output.begin",
                None,
                format!("iteration={iteration} bytes={read}"),
            );
        }
        let output = engine.process_output(&buffer[..read]);
        if trace_iteration {
            stage_diagnostic(
                "backpressure-handshake",
                "process_output.end",
                None,
                format!(
                    "iteration={iteration} parser_replies={}",
                    output.pty_writes.len()
                ),
            );
        }
        parser_replies = parser_replies.saturating_add(output.pty_writes.len());

        for (reply_index, response) in output.pty_writes.into_iter().enumerate() {
            if Instant::now() >= deadline {
                stage_diagnostic(
                    "backpressure-handshake",
                    "deadline.expired",
                    None,
                    format!("point=before-reply iteration={iteration} reply={reply_index}"),
                );
                return Err(handshake_deadline_error(
                    "before-parser-reply",
                    iteration,
                    bytes,
                    parser_replies,
                    &markers,
                    &sample,
                ));
            }
            let trace_reply = trace_iteration && reply_index == 0;
            if trace_reply {
                stage_diagnostic(
                    "backpressure-handshake",
                    "reply.begin",
                    None,
                    format!(
                        "iteration={iteration} reply={reply_index} bytes={}",
                        response.len()
                    ),
                );
            }
            let write_result = timeout_at(deadline, writer.as_mut().write_all(&response)).await;
            match write_result {
                Ok(Ok(())) => {
                    if trace_reply {
                        stage_diagnostic(
                            "backpressure-handshake",
                            "reply.end",
                            None,
                            format!("iteration={iteration} reply={reply_index} result=ok"),
                        );
                    }
                }
                Ok(Err(error)) => {
                    if trace_reply {
                        stage_diagnostic(
                            "backpressure-handshake",
                            "reply.end",
                            None,
                            format!(
                                "iteration={iteration} reply={reply_index} result=error kind={:?}",
                                error.kind()
                            ),
                        );
                    }
                    return Err(error).context("回复 PTY 终端查询");
                }
                Err(_) => {
                    stage_diagnostic(
                        "backpressure-handshake",
                        "reply.end",
                        None,
                        format!("iteration={iteration} reply={reply_index} result=timeout"),
                    );
                    return Err(handshake_deadline_error(
                        "parser-reply-pending",
                        iteration,
                        bytes,
                        parser_replies,
                        &markers,
                        &sample,
                    ));
                }
            }
        }

        bytes = bytes.saturating_add(read as u64);
        let remaining = OUTPUT_SAMPLE_LIMIT.saturating_sub(sample.len());
        sample.extend_from_slice(&buffer[..read.min(remaining)]);
        if trace_iteration {
            stage_diagnostic(
                "backpressure-handshake",
                "markers.observed",
                None,
                format!(
                    "iteration={iteration} ready={} stdout={} stderr={}",
                    markers.terminal_ready_seen, markers.stdout_seen, markers.stderr_seen
                ),
            );
        }
        if markers.all_seen() {
            if Instant::now() >= deadline {
                stage_diagnostic(
                    "backpressure-handshake",
                    "deadline.expired",
                    None,
                    format!("point=before-ready iteration={iteration} bytes={bytes}"),
                );
                return Err(handshake_deadline_error(
                    "before-ready-return",
                    iteration,
                    bytes,
                    parser_replies,
                    &markers,
                    &sample,
                ));
            }
            if observed_device_status_query(&sample) {
                ensure!(
                    parser_replies > 0,
                    "终端启动输出含真实 DSR 请求，但 parser 未生成任何 PTY 回复；bytes={bytes}"
                );
            }
            return Ok(HandshakeStats {
                bytes,
                sample,
                parser_replies,
            });
        }
        iteration = iteration.saturating_add(1);
    }
}

/// 统一握手 deadline 错误摘要；以固定大小计数和短前缀诊断原始输出，避免背压测试保留无限流量。
///
/// @author kongweiguang
fn handshake_deadline_error(
    stage: &str,
    iteration: usize,
    bytes: u64,
    parser_replies: usize,
    markers: &PtyOutputMarkerTracker,
    sample: &[u8],
) -> anyhow::Error {
    let (escaped, hexadecimal) = format_output_prefix(sample);
    anyhow!(
        "PTY 终端握手超过绝对 15s deadline；stage={stage}, iterations={iteration}, bytes={bytes}, parser_replies={parser_replies}, markers=ready:{},stdout:{},stderr:{}, longest_payload_runs=O:{},E:{}, prefix_escaped={escaped:?}, prefix_hex={hexadecimal}",
        markers.terminal_ready_seen,
        markers.stdout_seen,
        markers.stderr_seen,
        markers.longest_o_run,
        markers.longest_e_run
    )
}

/// 读取本测试 child 在 flush 完成后写入的输出进度；并发覆盖文件时保留可重试的空状态。
///
/// @author kongweiguang
fn read_backpressure_progress(path: &Path) -> Result<Option<u64>> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::WouldBlock
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error).context("读取背压 child 私有进度文件"),
    };
    Ok(content.trim().parse().ok())
}

/// 在 child 重写短进度值时短暂重试空读，避免把文件更新瞬间误判成无输出。
///
/// @author kongweiguang
async fn sample_backpressure_progress(path: &Path, deadline: Instant) -> Result<Option<u64>> {
    loop {
        if let Some(progress) = read_backpressure_progress(path)? {
            return Ok(Some(progress));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        sleep(Duration::from_millis(10)).await;
    }
}

/// 暂时恢复真实 PTY 消费，要求读到足够字节且 child 的 flush 进度越过暂停值后才重新停读。
///
/// 进度文件只证明 child 已完成两路 payload 写入；只有这些写入在停读时稳定、恢复读取后再推进，
/// 才能把队列背压与 ConPTY 的原始字节转换区分开。有限 deadline 同时约束 parser 回写。
///
/// @author kongweiguang
async fn consume_until_backpressure_releases(
    reader: &mut Pin<Box<dyn AsyncRead + Send + 'static>>,
    writer: &mut Pin<Box<dyn AsyncWrite + Send + 'static>>,
    engine: &mut TerminalEngine,
    progress_path: &Path,
    stalled_progress: u64,
    deadline: Instant,
) -> Result<(OutputStats, u64)> {
    let progress_before_read = read_backpressure_progress(progress_path)?
        .ok_or_else(|| anyhow!("恢复读取前 producer 进度不可读取"))?;
    ensure!(
        progress_before_read == stalled_progress,
        "恢复读取前 producer 已脱离稳定状态；expected={stalled_progress}, actual={progress_before_read}"
    );
    let mut buffer = [0_u8; 16 * 1024];
    let mut bytes = 0_u64;
    let mut sample = Vec::with_capacity(OUTPUT_SAMPLE_LIMIT);
    let mut parser_replies = 0_usize;
    let mut progress_advanced_after_read = false;

    loop {
        ensure!(
            Instant::now() < deadline,
            "恢复 PTY 消费后 producer 未能解除背压并达到负载门槛"
        );
        let read = timeout_at(deadline, reader.as_mut().read(&mut buffer))
            .await
            .map_err(|_| anyhow!("恢复 PTY 输出 consumer 超过绝对 deadline"))?
            .context("恢复 PTY 输出消费失败")?;
        ensure!(read > 0, "恢复消费时 PTY 在压力阶段提前关闭");

        let output = engine.process_output(&buffer[..read]);
        parser_replies = parser_replies.saturating_add(output.pty_writes.len());
        for response in output.pty_writes {
            timeout_at(deadline, writer.as_mut().write_all(&response))
                .await
                .map_err(|_| anyhow!("恢复消费时回复终端查询超过绝对 deadline"))?
                .context("恢复消费时回写终端查询失败")?;
        }

        bytes = bytes.saturating_add(read as u64);
        let remaining = OUTPUT_SAMPLE_LIMIT.saturating_sub(sample.len());
        sample.extend_from_slice(&buffer[..read.min(remaining)]);
        let Some(progress) = read_backpressure_progress(progress_path)? else {
            continue;
        };
        progress_advanced_after_read |= progress > stalled_progress;
        if progress_advanced_after_read
            && progress >= ACTIVE_OUTPUT_THRESHOLD
            && bytes >= ACTIVE_OUTPUT_THRESHOLD
        {
            if observed_device_status_query(&sample) {
                ensure!(
                    parser_replies > 0,
                    "恢复消费样本含真实 DSR 请求，但 parser 未生成 PTY 回复"
                );
            }
            return Ok((
                OutputStats {
                    bytes,
                    sample,
                    parser_replies,
                },
                progress,
            ));
        }
    }
}

/// 通过 Unix `ps` 查询仍在运行的测试 child；Windows 使用已固定的原生 process handle。
///
/// @author kongweiguang
#[cfg(unix)]
fn process_is_running(process_id: u32) -> Result<bool> {
    let pid = process_id.to_string();
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid])
        .output()
        .context("查询测试子进程 ps 状态")?;
    ensure!(
        output.status.success() || output.status.code() == Some(1),
        "ps 查询失败: {process_id}"
    );
    let state = String::from_utf8_lossy(&output.stdout);
    Ok(!state.trim().is_empty() && !state.trim_start().starts_with('Z'))
}

/// 在 deadline 前确认固定进程对象已退出，并显式固定异步结果的错误类型。
///
/// @author kongweiguang
async fn wait_for_process_exit(
    label: &'static str,
    process_probe: &ProcessExitProbe,
    deadline: Instant,
) -> Result<()> {
    let process_id = process_probe.process_id;
    stage_diagnostic(
        label,
        "process_exit.begin",
        Some(process_id),
        "poll child state",
    );
    let result = timeout_at(deadline, async {
        loop {
            if !process_is_running_until(process_probe, deadline).await? {
                return Ok::<(), anyhow::Error>(());
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| format!("{label} 进程仍未退出，pid={process_id}"))
    .and_then(|result| result);
    stage_diagnostic(
        label,
        "process_exit.end",
        Some(process_id),
        if result.is_ok() {
            "exited"
        } else {
            "pending or error"
        },
    );
    result
}

/// Windows 用零等待原生轮询避免逐 PID 启动 tasklist；Unix 保留既有 ps 查询与 deadline。
///
/// @author kongweiguang
#[cfg(windows)]
async fn process_is_running_until(
    process_probe: &ProcessExitProbe,
    deadline: Instant,
) -> Result<bool> {
    ensure!(
        Instant::now() < deadline,
        "等待测试 PID 状态查询超过 deadline"
    );
    process_probe.is_running()
}

/// 将 Unix 外部进程查询放入 blocking pool，避免阻塞 Tokio worker。
///
/// @author kongweiguang
#[cfg(unix)]
async fn process_is_running_until(
    process_probe: &ProcessExitProbe,
    deadline: Instant,
) -> Result<bool> {
    let process_id = process_probe.process_id;
    timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || process_is_running(process_id)),
    )
    .await
    .context("等待测试 PID 状态查询超时")?
    .context("测试 PID 状态查询任务失败")?
}

/// 验证持续双流输出在消费状态下关闭，且关闭后输出桥接 worker 和 child 都退出。
///
/// @author kongweiguang
async fn close_during_consumed_output(
    factory: &LocalPtySessionTransportFactory,
    config_root: &Path,
    total_deadline: Instant,
) -> Result<()> {
    stage_diagnostic(
        "continuous-output",
        "scenario.begin",
        None,
        format!("minimum_bytes={ACTIVE_OUTPUT_THRESHOLD}"),
    );
    ensure!(
        Instant::now() < total_deadline,
        "持续输出场景启动前已超过总 deadline"
    );
    let mut probe = open_owned_pty(factory, "continuous-output", flood_target(config_root)).await?;
    let process_id = probe.process_id;
    let engine = match create_terminal_engine() {
        Ok(engine) => engine,
        Err(error) => return return_after_owned_cleanup(&mut probe, error).await,
    };
    let counter = Arc::new(AtomicU64::new(0));
    let reader = match probe.take_reader() {
        Ok(reader) => reader,
        Err(error) => return return_after_owned_cleanup(&mut probe, error).await,
    };
    let writer = match probe.writer.take() {
        Some(writer) => writer,
        None => {
            return return_after_owned_cleanup(
                &mut probe,
                anyhow!("持续输出 PTY writer 已提前释放，pid={process_id}"),
            )
            .await;
        }
    };
    let mut consumer = tokio::spawn(consume_terminal_output(
        reader,
        writer,
        Arc::clone(&counter),
        Arc::clone(&probe.close_started),
        engine,
        "continuous-output",
        process_id,
    ));
    stage_diagnostic(
        "continuous-output",
        "output_threshold.begin",
        Some(process_id),
        format!("minimum_bytes={ACTIVE_OUTPUT_THRESHOLD}"),
    );
    let output_deadline = stage_deadline(total_deadline, Duration::from_secs(15));
    let output_result =
        wait_for_output_bytes(&counter, ACTIVE_OUTPUT_THRESHOLD, output_deadline).await;
    stage_diagnostic(
        "continuous-output",
        "output_threshold.end",
        Some(process_id),
        format!(
            "bytes={} result={}",
            counter.load(Ordering::Acquire),
            if output_result.is_ok() { "ok" } else { "error" }
        ),
    );
    let child_alive_before_close = if output_result.is_err() {
        Some(match probe.process_probe() {
            Ok(process_probe) => {
                process_is_running_until(
                    process_probe,
                    stage_deadline(total_deadline, Duration::from_secs(3)),
                )
                .await
            }
            Err(error) => Err(error),
        })
    } else {
        None
    };

    // 即便字节门槛失败，也先关闭 child 并 join reader，防止断言路径遗留本测试进程。
    let close_deadline = stage_deadline(total_deadline, CLOSE_DEADLINE);
    stage_diagnostic(
        "continuous-output",
        "close_sequence.begin",
        Some(process_id),
        "first close",
    );
    let first_close = probe.close_until(close_deadline).await;
    let close_and_wait_result = match first_close {
        Ok(status) => Ok(status),
        Err(error) => match probe.close_until(Instant::now() + CLOSE_DEADLINE).await {
            Ok(_) => Err(error.context("首次关闭失败；后续回收已完成")),
            Err(cleanup_error) => {
                Err(error.context(format!("首次关闭和后续回收均失败: {cleanup_error:#}")))
            }
        },
    };
    let child_alive_after_close = if output_result.is_err() {
        Some(match probe.process_probe() {
            Ok(process_probe) => {
                process_is_running_until(
                    process_probe,
                    stage_deadline(total_deadline, Duration::from_secs(3)),
                )
                .await
            }
            Err(error) => Err(error),
        })
    } else {
        None
    };
    let consumer_result = join_output_consumer(
        &mut consumer,
        Instant::now() + CLOSE_DEADLINE,
        "continuous-output",
        process_id,
    )
    .await;
    stage_diagnostic(
        "continuous-output",
        "close_sequence.end",
        Some(process_id),
        format!(
            "close={} consumer={}",
            if close_and_wait_result.is_ok() {
                "ok"
            } else {
                "error"
            },
            if consumer_result.is_ok() {
                "joined"
            } else {
                "error"
            }
        ),
    );

    let result = async {
        if let Err(output_error) = output_result {
            let (prefix_escaped, prefix_hex) = match consumer_result.as_ref() {
                Ok(stats) => format_output_prefix(&stats.sample),
                Err(_) => ("<consumer-unavailable>".to_owned(), "<consumer-unavailable>".to_owned()),
            };
            let output_bytes = consumer_result
                .as_ref()
                .map(|stats| stats.bytes)
                .unwrap_or_else(|_| counter.load(Ordering::Acquire));
            let parser_replies = consumer_result
                .as_ref()
                .map(|stats| stats.parser_replies)
                .unwrap_or_default();
            let child_state = match child_alive_before_close.as_ref() {
                Some(Ok(alive)) => format!("alive={alive}"),
                Some(Err(error)) => format!("unavailable={error:#}"),
                None => "not-queried".to_owned(),
            };
            let child_after_close_state = match child_alive_after_close.as_ref() {
                Some(Ok(alive)) => format!("alive={alive}"),
                Some(Err(error)) => format!("unavailable={error:#}"),
                None => "not-queried".to_owned(),
            };
            let consumer_state = match consumer_result.as_ref() {
                Ok(_) => "joined".to_owned(),
                Err(error) => format!("failed={error:#}"),
            };
            return Err(output_error.context(format!(
                "PTY 输出超时诊断: consumer_bytes_after_close={output_bytes}, parser_replies={parser_replies}, child_before_close={child_state}, child_after_close={child_after_close_state}, prefix_escaped={prefix_escaped:?}, prefix_hex={prefix_hex}, close_and_wait={close_and_wait_result:?}, consumer={consumer_state}"
            )));
        }
        let stats = consumer_result?;
        let sample = String::from_utf8_lossy(&stats.sample);
        ensure!(
            stats.bytes >= ACTIVE_OUTPUT_THRESHOLD,
            "continuous output 样本不足: label={}, pid={}, bytes={}",
            probe.label,
            process_id,
            stats.bytes
        );
        if observed_device_status_query(&stats.sample) {
            ensure!(
                stats.parser_replies > 0,
                "PTY 样本含真实 DSR 请求，但 parser 未写回响应: label={}, pid={}, bytes={}",
                probe.label,
                process_id,
                stats.bytes
            );
        }
        ensure!(
            sample.contains(TERMINAL_READY_MARKER)
                && sample.contains("KERMINAL_STDOUT_LOAD")
                && sample.contains("KERMINAL_STDERR_LOAD"),
            "PTY 样本未同时观察到 stdout/stderr 标记: label={}, pid={}, bytes={}",
            probe.label,
            process_id,
            stats.bytes
        );
        close_and_wait_result.with_context(|| format!("关闭持续输出 PTY，pid={process_id}"))?;
        wait_for_process_exit(
            "continuous-output",
            probe.process_probe()?,
            stage_deadline(total_deadline, Duration::from_secs(5)),
        )
        .await
    }
    .await;
    stage_diagnostic(
        "continuous-output",
        "scenario.end",
        Some(process_id),
        if result.is_ok() { "ok" } else { "error" },
    );
    match result {
        Ok(()) => Ok(()),
        Err(error) => return_after_owned_cleanup(&mut probe, error).await,
    }
}

/// 先握手，再用 producer 进度“推进—稳定—恢复读取后再推进”证明背压，最后停读关闭并 drain。
///
/// @author kongweiguang
async fn close_with_output_consumer_backpressure(
    factory: &LocalPtySessionTransportFactory,
    config_root: &Path,
    total_deadline: Instant,
) -> Result<()> {
    stage_diagnostic(
        "backpressure",
        "scenario.begin",
        None,
        "hold reader then close",
    );
    ensure!(
        Instant::now() < total_deadline,
        "背压场景启动前已超过总 deadline"
    );
    let target = backpressure_target(config_root)?;
    let progress_path = target.progress_path.clone();
    let start_path = target.start_path.clone();
    let mut probe = open_owned_pty(factory, "backpressured-output", target.target).await?;
    let process_id = probe.process_id;
    stage_diagnostic(
        "backpressure",
        "handshake.begin",
        Some(process_id),
        "answer terminal queries",
    );
    stage_diagnostic(
        "backpressure",
        "engine_create.begin",
        Some(process_id),
        "create terminal parser before consuming output",
    );
    let engine_result = create_terminal_engine();
    stage_diagnostic(
        "backpressure",
        "engine_create.end",
        Some(process_id),
        if engine_result.is_ok() { "ok" } else { "error" },
    );
    let mut engine = match engine_result {
        Ok(engine) => engine,
        Err(error) => {
            stage_diagnostic(
                "backpressure",
                "handshake.end",
                Some(process_id),
                "terminal engine creation error",
            );
            return return_after_owned_cleanup(&mut probe, error).await;
        }
    };
    let handshake_result = match (probe.reader.as_mut(), probe.writer.as_mut()) {
        (Some(reader), Some(writer)) => {
            answer_terminal_queries_until_ready(
                reader,
                writer,
                &mut engine,
                stage_deadline(total_deadline, Duration::from_secs(15)),
            )
            .await
        }
        _ => Err(anyhow!("背压场景 PTY 流已提前释放，pid={process_id}")),
    };
    let handshake = match handshake_result {
        Ok(handshake) => {
            stage_diagnostic(
                "backpressure",
                "handshake.end",
                Some(process_id),
                format!(
                    "bytes={} parser_replies={}",
                    handshake.bytes, handshake.parser_replies
                ),
            );
            handshake
        }
        Err(error) => {
            stage_diagnostic(
                "backpressure",
                "handshake.end",
                Some(process_id),
                "result=error",
            );
            let progress_at_failure = read_backpressure_progress(&progress_path).ok().flatten();
            let error = error.context(format!(
                "背压握手失败前 child 已完成的 payload 字节数={progress_at_failure:?}"
            ));
            return return_after_owned_cleanup(&mut probe, error).await;
        }
    };
    stage_diagnostic(
        "backpressure",
        "progress_before_start.begin",
        Some(process_id),
        "verify producer is still gated",
    );
    let progress_before_start = sample_backpressure_progress(
        &progress_path,
        stage_deadline(total_deadline, Duration::from_millis(250)),
    )
    .await;
    stage_diagnostic(
        "backpressure",
        "progress_before_start.end",
        Some(process_id),
        format!(
            "value={:?}",
            progress_before_start.as_ref().ok().and_then(|value| *value)
        ),
    );
    let progress_before_start = match progress_before_start {
        Ok(Some(progress)) if progress == 0 => progress,
        Ok(Some(progress)) => {
            return return_after_owned_cleanup(
                &mut probe,
                anyhow!("producer 在测试释放 start gate 前已运行；progress={progress}"),
            )
            .await;
        }
        Ok(None) => {
            return return_after_owned_cleanup(
                &mut probe,
                anyhow!("producer 未创建初始进度文件，无法验证 start gate"),
            )
            .await;
        }
        Err(error) => return return_after_owned_cleanup(&mut probe, error).await,
    };

    stage_diagnostic(
        "backpressure",
        "start_gate.release.begin",
        Some(process_id),
        "allow the two-stream producer to start",
    );
    if let Err(error) = std::fs::write(&start_path, b"start") {
        return return_after_owned_cleanup(
            &mut probe,
            anyhow!(error).context("释放背压测试的私有 producer gate"),
        )
        .await;
    }
    stage_diagnostic(
        "backpressure",
        "start_gate.release.end",
        Some(process_id),
        "released",
    );

    // Gate 后不再读取；producer 必须先写出数据，随后在有界 channel/ConPTY 背压处稳定。
    let pause_deadline = stage_deadline(total_deadline, Duration::from_millis(1_500));
    stage_diagnostic(
        "backpressure",
        "consumer_pause_1.begin",
        Some(process_id),
        "1.5s no reader after producer gate",
    );
    let pause_result = if Instant::now() < total_deadline {
        timeout_at(pause_deadline, sleep(Duration::from_millis(1_500)))
            .await
            .map_err(|_| anyhow!("背压准备超过总 deadline"))
    } else {
        Err(anyhow!("背压阶段启动前已超过总 deadline"))
    };
    stage_diagnostic(
        "backpressure",
        "consumer_pause_1.end",
        Some(process_id),
        if pause_result.is_ok() {
            "completed"
        } else {
            "deadline"
        },
    );
    let progress_after_pause_1 = sample_backpressure_progress(
        &progress_path,
        stage_deadline(total_deadline, Duration::from_millis(250)),
    )
    .await;
    stage_diagnostic(
        "backpressure",
        "progress_after_pause_1.end",
        Some(process_id),
        format!(
            "value={:?}",
            progress_after_pause_1
                .as_ref()
                .ok()
                .and_then(|value| *value)
        ),
    );
    let first_stability_result = if pause_result.is_ok() && Instant::now() < total_deadline {
        match timeout_at(
            stage_deadline(total_deadline, BACKPRESSURE_STABILITY_WINDOW),
            sleep(BACKPRESSURE_STABILITY_WINDOW),
        )
        .await
        {
            Ok(()) => Ok(()),
            Err(_) => Err(anyhow!("首次背压 producer 稳定观测超过总 deadline")),
        }
    } else {
        Err(anyhow!("背压 producer 未完成首次暂停窗口"))
    };
    let progress_after_pause_1_stability = sample_backpressure_progress(
        &progress_path,
        stage_deadline(total_deadline, Duration::from_millis(250)),
    )
    .await;
    stage_diagnostic(
        "backpressure",
        "progress_after_pause_1_stability.end",
        Some(process_id),
        format!(
            "value={:?}",
            progress_after_pause_1_stability
                .as_ref()
                .ok()
                .and_then(|value| *value)
        ),
    );

    let initial_pause_values = match (progress_after_pause_1, progress_after_pause_1_stability) {
        (Ok(Some(after_pause)), Ok(Some(after_stability))) => Ok((after_pause, after_stability)),
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => Err(anyhow!("首次暂停窗口内 producer 进度不可读取")),
    };
    let (progress_after_pause_1, progress_after_pause_1_stability) = match initial_pause_values {
        Ok(values) => values,
        Err(error) => return return_after_owned_cleanup(&mut probe, error).await,
    };
    if let Err(error) = pause_result.as_ref() {
        return return_after_owned_cleanup(&mut probe, anyhow!("背压首次暂停失败: {error:#}"))
            .await;
    }
    if let Err(error) = first_stability_result.as_ref() {
        return return_after_owned_cleanup(&mut probe, anyhow!("背压首次稳定窗口失败: {error:#}"))
            .await;
    }
    if progress_after_pause_1 <= progress_before_start {
        return return_after_owned_cleanup(
            &mut probe,
            anyhow!(
                "释放 gate 后 producer 未在停读期间推进；before={progress_before_start}, after={progress_after_pause_1}"
            ),
        )
        .await;
    }
    if progress_after_pause_1 != progress_after_pause_1_stability {
        return return_after_owned_cleanup(
            &mut probe,
            anyhow!(
                "首次停读期间 producer 尚未稳定，无法证明背压；pause={progress_after_pause_1}, stable={progress_after_pause_1_stability}"
            ),
        )
        .await;
    }

    let (reader, writer) = match (probe.reader.as_mut(), probe.writer.as_mut()) {
        (Some(reader), Some(writer)) => (reader, writer),
        _ => {
            return return_after_owned_cleanup(
                &mut probe,
                anyhow!("背压解除验证开始前 PTY 流已提前释放，pid={process_id}"),
            )
            .await;
        }
    };
    stage_diagnostic(
        "backpressure",
        "consumer_resume.begin",
        Some(process_id),
        format!("resume reading after producer stalled at {progress_after_pause_1_stability}"),
    );
    let resumed = consume_until_backpressure_releases(
        reader,
        writer,
        &mut engine,
        &progress_path,
        progress_after_pause_1_stability,
        stage_deadline(total_deadline, Duration::from_secs(15)),
    )
    .await;
    let (resume_stats, progress_after_resume) = match resumed {
        Ok(result) => result,
        Err(error) => return return_after_owned_cleanup(&mut probe, error).await,
    };
    stage_diagnostic(
        "backpressure",
        "consumer_resume.end",
        Some(process_id),
        format!(
            "read_bytes={} parser_replies={} producer_progress={progress_after_resume}",
            resume_stats.bytes, resume_stats.parser_replies
        ),
    );

    // 读端确实解除一次背压后再次停读；第二次稳定证明关闭开始时 producer 又受到了反压。
    let second_pause_deadline = stage_deadline(total_deadline, Duration::from_millis(1_500));
    stage_diagnostic(
        "backpressure",
        "consumer_pause_2.begin",
        Some(process_id),
        "1.5s no reader after producer resumed",
    );
    let second_pause_result =
        timeout_at(second_pause_deadline, sleep(Duration::from_millis(1_500)))
            .await
            .map_err(|_| anyhow!("背压第二次暂停超过总 deadline"));
    stage_diagnostic(
        "backpressure",
        "consumer_pause_2.end",
        Some(process_id),
        if second_pause_result.is_ok() {
            "completed"
        } else {
            "deadline"
        },
    );
    let progress_after_pause_2 = sample_backpressure_progress(
        &progress_path,
        stage_deadline(total_deadline, Duration::from_millis(250)),
    )
    .await;
    let second_stability_result = if second_pause_result.is_ok() && Instant::now() < total_deadline
    {
        match timeout_at(
            stage_deadline(total_deadline, BACKPRESSURE_STABILITY_WINDOW),
            sleep(BACKPRESSURE_STABILITY_WINDOW),
        )
        .await
        {
            Ok(()) => Ok(()),
            Err(_) => Err(anyhow!("第二次背压 producer 稳定观测超过总 deadline")),
        }
    } else {
        Err(anyhow!("背压 producer 未完成第二次暂停窗口"))
    };
    let progress_after_pause_2_stability = sample_backpressure_progress(
        &progress_path,
        stage_deadline(total_deadline, Duration::from_millis(250)),
    )
    .await;
    stage_diagnostic(
        "backpressure",
        "progress_after_pause_2.end",
        Some(process_id),
        format!(
            "pause={:?} stable={:?}",
            progress_after_pause_2
                .as_ref()
                .ok()
                .and_then(|value| *value),
            progress_after_pause_2_stability
                .as_ref()
                .ok()
                .and_then(|value| *value)
        ),
    );
    let (progress_after_pause_2, progress_after_pause_2_stability) =
        match (progress_after_pause_2, progress_after_pause_2_stability) {
            (Ok(Some(paused)), Ok(Some(stable))) => (paused, stable),
            (Err(error), _) | (_, Err(error)) => {
                return return_after_owned_cleanup(&mut probe, error).await;
            }
            _ => {
                return return_after_owned_cleanup(
                    &mut probe,
                    anyhow!("第二次暂停窗口内 producer 进度不可读取"),
                )
                .await;
            }
        };
    if let Err(error) = second_pause_result.as_ref() {
        return return_after_owned_cleanup(&mut probe, anyhow!("背压第二次暂停失败: {error:#}"))
            .await;
    }
    if let Err(error) = second_stability_result.as_ref() {
        return return_after_owned_cleanup(
            &mut probe,
            anyhow!("背压第二次稳定窗口失败: {error:#}"),
        )
        .await;
    }
    let progress_stable = progress_after_pause_2 == progress_after_pause_2_stability;

    let alive_before_close = match probe.process_probe() {
        Ok(process_probe) => {
            process_is_running_until(
                process_probe,
                stage_deadline(total_deadline, Duration::from_secs(3)),
            )
            .await
        }
        Err(error) => Err(error),
    };
    stage_diagnostic(
        "backpressure",
        "child_state.end",
        Some(process_id),
        format!("alive={alive_before_close:?}, producer_stable={progress_stable}"),
    );

    stage_diagnostic(
        "backpressure",
        "writer_shutdown.begin",
        Some(process_id),
        "shutdown PTY input while output reader is paused",
    );
    let writer_shutdown = match probe.writer.as_mut() {
        Some(writer) => match timeout_at(
            stage_deadline(total_deadline, Duration::from_secs(3)),
            writer.as_mut().shutdown(),
        )
        .await
        {
            Ok(result) => result.context("关闭背压场景 PTY 输入端"),
            Err(_) => Err(anyhow!(
                "背压场景 PTY writer shutdown 超时，pid={process_id}"
            )),
        },
        None => Err(anyhow!("背压场景 PTY writer 已提前释放，pid={process_id}")),
    };
    stage_diagnostic(
        "backpressure",
        "writer_shutdown.end",
        Some(process_id),
        if writer_shutdown.is_ok() {
            "ok"
        } else {
            "error"
        },
    );
    let first_deadline = stage_deadline(total_deadline, Duration::from_secs(3));
    // 首次关闭和 wait_exit 都保留未消费 reader，专门验证 ConPTY close 是否依赖输出 drain。
    probe.close_started.store(true, Ordering::Release);
    stage_diagnostic(
        "backpressure",
        "first_close.begin",
        Some(process_id),
        "reader remains paused",
    );
    let first_close = timeout_at(first_deadline, probe.control.close_until(first_deadline)).await;
    stage_diagnostic(
        "backpressure",
        "first_close.end",
        Some(process_id),
        if matches!(&first_close, Ok(Ok(()))) {
            "ok"
        } else {
            "error or timeout"
        },
    );
    stage_diagnostic(
        "backpressure",
        "first_wait_exit.begin",
        Some(process_id),
        "shared deadline",
    );
    let first_exit = timeout_at(first_deadline, probe.control.wait_exit()).await;
    stage_diagnostic(
        "backpressure",
        "first_wait_exit.end",
        Some(process_id),
        if matches!(&first_exit, Ok(Ok(_))) {
            "ok"
        } else {
            "error or timeout"
        },
    );

    let reader = match probe.take_reader() {
        Ok(reader) => reader,
        Err(error) => return return_after_owned_cleanup(&mut probe, error).await,
    };
    let writer = match probe.writer.take() {
        Some(writer) => writer,
        None => {
            return return_after_owned_cleanup(
                &mut probe,
                anyhow!("关闭后 drain PTY writer 已提前释放，pid={process_id}"),
            )
            .await;
        }
    };
    let drain_counter = Arc::new(AtomicU64::new(0));
    stage_diagnostic(
        "backpressure",
        "drain.begin",
        Some(process_id),
        "resume reader after first close",
    );
    let mut drain = tokio::spawn(consume_terminal_output(
        reader,
        writer,
        Arc::clone(&drain_counter),
        Arc::clone(&probe.close_started),
        engine,
        "backpressure-drain",
        process_id,
    ));
    let (recovered_close, recovered_exit) =
        if matches!(&first_close, Ok(Ok(()))) && matches!(&first_exit, Ok(Ok(_))) {
            probe.closed = true;
            (None, None)
        } else {
            let recovery_deadline = Instant::now() + CLOSE_DEADLINE;
            // 初次关闭已到界后启动有界输出 drain，并由同一个 control owner 继续关闭与 join。
            stage_diagnostic(
                "backpressure",
                "recovery_close.begin",
                Some(process_id),
                "after drain start",
            );
            let close = timeout_at(
                recovery_deadline,
                probe.control.close_until(recovery_deadline),
            )
            .await;
            stage_diagnostic(
                "backpressure",
                "recovery_close.end",
                Some(process_id),
                if matches!(&close, Ok(Ok(()))) {
                    "ok"
                } else {
                    "error or timeout"
                },
            );
            stage_diagnostic(
                "backpressure",
                "recovery_wait_exit.begin",
                Some(process_id),
                "shared recovery deadline",
            );
            let exit = timeout_at(recovery_deadline, probe.control.wait_exit()).await;
            stage_diagnostic(
                "backpressure",
                "recovery_wait_exit.end",
                Some(process_id),
                if matches!(&exit, Ok(Ok(_))) {
                    "ok"
                } else {
                    "error or timeout"
                },
            );
            if matches!(&close, Ok(Ok(()))) && matches!(&exit, Ok(Ok(_))) {
                probe.closed = true;
            }
            (Some(close), Some(exit))
        };
    let drain_deadline = Instant::now() + CLOSE_DEADLINE;
    stage_diagnostic(
        "backpressure",
        "drain_join.begin",
        Some(process_id),
        "bounded close drain",
    );
    let drain_result = match timeout_at(drain_deadline, &mut drain).await {
        Ok(joined) => joined
            .context("关闭后 drain task 被取消")
            .and_then(|result| result.context("关闭后 drain PTY 输出失败")),
        Err(_) => {
            stage_diagnostic(
                "backpressure",
                "drain_abort.begin",
                Some(process_id),
                "12s deadline elapsed",
            );
            drain.abort();
            let abort_join = timeout_at(Instant::now() + Duration::from_secs(2), &mut drain).await;
            stage_diagnostic(
                "backpressure",
                "drain_abort.end",
                Some(process_id),
                if abort_join.is_ok() {
                    "joined within 2s"
                } else {
                    "join still pending after 2s"
                },
            );
            Err(anyhow!(
                "关闭后 drain PTY 输出超时；abort_join={}",
                if abort_join.is_ok() {
                    "completed"
                } else {
                    "pending"
                }
            ))
        }
    };
    let drain_bytes = drain_result.as_ref().ok().map(|stats| stats.bytes);
    stage_diagnostic(
        "backpressure",
        "drain_join.end",
        Some(process_id),
        format!(
            "result={} bytes={:?}",
            if drain_result.is_ok() { "ok" } else { "error" },
            drain_bytes
        ),
    );

    let (handshake_prefix_escaped, handshake_prefix_hex) = format_output_prefix(&handshake.sample);
    let (resume_prefix_escaped, resume_prefix_hex) = format_output_prefix(&resume_stats.sample);
    let (drain_prefix_escaped, drain_prefix_hex) = match drain_result.as_ref() {
        Ok(stats) => format_output_prefix(&stats.sample),
        Err(_) => (
            "<drain-unavailable>".to_owned(),
            "<drain-unavailable>".to_owned(),
        ),
    };
    let drained_bytes = drain_bytes;
    let diagnostic = format!(
        "pid={process_id}, handshake_bytes={}, handshake_parser_replies={}, handshake_prefix_escaped={handshake_prefix_escaped:?}, handshake_prefix_hex={handshake_prefix_hex}, progress_before_start={progress_before_start}, progress_after_pause_1={progress_after_pause_1}, progress_after_pause_1_stability={progress_after_pause_1_stability}, progress_after_resume={progress_after_resume}, resume_bytes={}, resume_parser_replies={}, resume_prefix_escaped={resume_prefix_escaped:?}, resume_prefix_hex={resume_prefix_hex}, progress_after_pause_2={progress_after_pause_2}, progress_after_pause_2_stability={progress_after_pause_2_stability}, progress_stable={progress_stable}, alive_before_close={alive_before_close:?}, writer_shutdown={writer_shutdown:?}, first_close={first_close:?}, first_exit={first_exit:?}, recovery_close={recovered_close:?}, recovery_exit={recovered_exit:?}, drained_bytes={drained_bytes:?}, drain_prefix_escaped={drain_prefix_escaped:?}, drain_prefix_hex={drain_prefix_hex}",
        handshake.bytes, handshake.parser_replies, resume_stats.bytes, resume_stats.parser_replies,
    );

    let result = async {
        let alive_before_close = alive_before_close
            .as_ref()
            .map_err(|error| anyhow!("检查背压场景 child 状态失败: {error:#}; {diagnostic}"))?;
        ensure!(
            *alive_before_close,
            "背压期间的 PTY child 已提前退出；{diagnostic}"
        );
        ensure!(
            writer_shutdown.is_ok(),
            "背压场景 writer shutdown 失败；{diagnostic}"
        );
        ensure!(
            progress_after_pause_1 > progress_before_start,
            "初次停读期间 producer 未推进；{diagnostic}"
        );
        ensure!(
            progress_after_pause_1 == progress_after_pause_1_stability,
            "初次停读期间 producer 未稳定，未证明输出端形成背压；{diagnostic}"
        );
        let drained = drain_result
            .as_ref()
            .map_err(|error| anyhow!("关闭后 drain PTY 输出失败: {error:#}; {diagnostic}"))?;
        let consumed_output_bytes = handshake
            .bytes
            .saturating_add(resume_stats.bytes)
            .saturating_add(drained.bytes);
        ensure!(
            progress_after_resume > progress_after_pause_1_stability,
            "恢复真实 PTY 消费后 producer 进度未越过暂停值；{diagnostic}"
        );
        ensure!(
            progress_after_resume >= ACTIVE_OUTPUT_THRESHOLD,
            "producer 总 payload flush 负载不足 64 KiB；{diagnostic}"
        );
        ensure!(
            resume_stats.bytes >= ACTIVE_OUTPUT_THRESHOLD,
            "解除背压阶段真实 PTY 消费字节不足 64 KiB；{diagnostic}"
        );
        ensure!(
            progress_stable,
            "第二次停读期间 producer 进度仍在推进，关闭前未形成稳定背压；{diagnostic}"
        );
        ensure!(
            drained.bytes > 0,
            "关闭时已形成背压，但恢复读取后没有拿到任何排队输出；{diagnostic}"
        );
        ensure!(
            consumed_output_bytes >= ACTIVE_OUTPUT_THRESHOLD,
            "压力场景端到端消费负载不足 64 KiB；consumed={consumed_output_bytes}; {diagnostic}"
        );
        let recovered = recovered_close
            .as_ref()
            .is_some_and(|close| matches!(close, Ok(Ok(()))))
            && recovered_exit
                .as_ref()
                .is_some_and(|exit| matches!(exit, Ok(Ok(_))));
        ensure!(
            matches!(&first_close, Ok(Ok(()))) && matches!(&first_exit, Ok(Ok(_))),
            "PTY close/wait_exit 在未消费 reader 時未能按初始 deadline 完成；owner recovery={recovered}; {diagnostic}"
        );
        wait_for_process_exit(
            "backpressured-output",
            probe.process_probe()?,
            stage_deadline(total_deadline, Duration::from_secs(5)),
        )
        .await
        .with_context(|| format!("背压 child 退出状态未确认；{diagnostic}"))
    }
    .await;
    stage_diagnostic(
        "backpressure",
        "scenario.end",
        Some(process_id),
        if result.is_ok() { "ok" } else { "error" },
    );
    match result {
        Ok(()) => Ok(()),
        Err(error) => return_after_owned_cleanup(&mut probe, error).await,
    }
}

/// 用一个真实 child 验证关闭控制句柄和退出等待可安全重复调用。
///
/// @author kongweiguang
async fn close_repeatedly(
    factory: &LocalPtySessionTransportFactory,
    config_root: &Path,
    total_deadline: Instant,
) -> Result<()> {
    stage_diagnostic(
        "repeat-close",
        "scenario.begin",
        None,
        "close same owner twice",
    );
    ensure!(
        Instant::now() < total_deadline,
        "重复关闭场景启动前已超过总 deadline"
    );
    let mut probe = open_owned_pty(factory, "repeat-close", idle_target(config_root)).await?;
    let process_id = probe.process_id;
    let result = async {
        stage_diagnostic(
            "repeat-close",
            "first_close.begin",
            Some(process_id),
            "close owner",
        );
        let first = probe
            .close_until(stage_deadline(total_deadline, CLOSE_DEADLINE))
            .await
            .context("首次关闭重复调用 PTY")?;
        stage_diagnostic("repeat-close", "first_close.end", Some(process_id), "ok");
        stage_diagnostic(
            "repeat-close",
            "second_close.begin",
            Some(process_id),
            "same owner",
        );
        let second = probe
            .close_until(Instant::now() + CLOSE_DEADLINE)
            .await
            .context("重复关闭同一个 PTY")?;
        stage_diagnostic("repeat-close", "second_close.end", Some(process_id), "ok");
        ensure!(
            first == second,
            "重复 wait_exit 状态不一致: pid={process_id}, first={first:?}, second={second:?}"
        );
        wait_for_process_exit(
            "repeat-close",
            probe.process_probe()?,
            stage_deadline(total_deadline, Duration::from_secs(5)),
        )
        .await
    }
    .await;
    stage_diagnostic(
        "repeat-close",
        "scenario.end",
        Some(process_id),
        if result.is_ok() { "ok" } else { "error" },
    );
    match result {
        Ok(()) => Ok(()),
        Err(error) => return_after_owned_cleanup(&mut probe, error).await,
    }
}

/// 并发关闭多个独立 PTY，确认一个 session 的资源回收不阻塞其他 child 的清理。
///
/// @author kongweiguang
async fn close_concurrently(
    factory: &LocalPtySessionTransportFactory,
    config_root: &Path,
    total_deadline: Instant,
) -> Result<()> {
    stage_diagnostic(
        "concurrent-close",
        "scenario.begin",
        None,
        format!("session_count={CONCURRENT_CLOSE_COUNT}"),
    );
    let mut probes = Vec::with_capacity(CONCURRENT_CLOSE_COUNT);
    for _ in 0..CONCURRENT_CLOSE_COUNT {
        if Instant::now() >= total_deadline {
            let cleanup_failures = close_all_owned(&mut probes).await;
            return Err(anyhow!(
                "并发关闭场景创建 PTY 时超过总 deadline；cleanup_failures={cleanup_failures:?}"
            ));
        }
        match open_owned_pty(factory, "concurrent-close", idle_target(config_root)).await {
            Ok(probe) => probes.push(probe),
            Err(error) => {
                let cleanup_failures = close_all_owned(&mut probes).await;
                return Err(error.context(if cleanup_failures.is_empty() {
                    "并发场景打开失败；此前已创建 PTY 均已清理".to_owned()
                } else {
                    format!(
                        "并发场景打开失败；已有 PTY 清理错误: {}",
                        cleanup_failures.join("; ")
                    )
                }));
            }
        }
    }
    let deadline = stage_deadline(total_deadline, CLOSE_DEADLINE);
    let mut tasks = JoinSet::new();
    for (index, mut probe) in probes.into_iter().enumerate() {
        tasks.spawn(async move {
            let process_id = probe.process_id;
            let first = probe.close_until(deadline).await;
            let result = match first {
                Ok(status) => Ok(status),
                Err(error) => match probe.close_until(Instant::now() + CLOSE_DEADLINE).await {
                    Ok(_) => Err(error.context("首次并发关闭失败；后续清理已完成")),
                    Err(cleanup) => {
                        Err(error.context(format!("并发关闭后续清理失败: {cleanup:#}")))
                    }
                },
            }
            .with_context(|| format!("并发关闭第 {index} 个 PTY，pid={process_id}"));
            let process_probe = probe.take_process_probe();
            (index, process_id, result, process_probe)
        });
    }

    let mut failures = Vec::new();
    let mut process_probes = Vec::with_capacity(CONCURRENT_CLOSE_COUNT);
    loop {
        stage_diagnostic(
            "concurrent-close",
            "join_next.begin",
            None,
            format!("remaining={}", tasks.len()),
        );
        let joined = match timeout_at(deadline, tasks.join_next()).await {
            Ok(joined) => joined,
            Err(_) => {
                stage_diagnostic(
                    "concurrent-close",
                    "join_next.timeout",
                    None,
                    format!("remaining={}", tasks.len()),
                );
                tasks.abort_all();
                let abort_deadline = Instant::now() + Duration::from_secs(2);
                let mut joined_after_abort = true;
                while !tasks.is_empty() {
                    match timeout_at(abort_deadline, tasks.join_next()).await {
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                        Err(_) => {
                            joined_after_abort = false;
                            break;
                        }
                    }
                }
                stage_diagnostic(
                    "concurrent-close",
                    "abort_join.end",
                    None,
                    if joined_after_abort {
                        "joined within 2s"
                    } else {
                        "join still pending after 2s"
                    },
                );
                failures.push(format!(
                    "并发关闭 JoinSet 超过总 deadline；abort_join={}",
                    if joined_after_abort {
                        "completed"
                    } else {
                        "pending"
                    }
                ));
                break;
            }
        };
        stage_diagnostic(
            "concurrent-close",
            "join_next.end",
            None,
            format!("remaining={}", tasks.len()),
        );
        let Some(joined) = joined else { break };
        match joined {
            Ok((index, process_id, result, process_probe)) => {
                match process_probe {
                    Ok(process_probe) => process_probes.push(process_probe),
                    Err(error) => failures.push(format!(
                        "index={index}, pid={process_id}: 无法保留进程身份句柄: {error:#}"
                    )),
                }
                if let Err(error) = result {
                    failures.push(format!("index={index}, pid={process_id}: {error:#}"));
                }
            }
            Err(error) => failures.push(format!("close task join failed: {error}")),
        }
    }

    let exit_deadline = stage_deadline(total_deadline, Duration::from_secs(8));
    for process_probe in process_probes {
        let process_id = process_probe.process_id;
        stage_diagnostic(
            "concurrent-close",
            "wait_process_exit.begin",
            Some(process_id),
            "poll child",
        );
        match wait_for_process_exit("concurrent-close", &process_probe, exit_deadline).await {
            Ok(()) => stage_diagnostic(
                "concurrent-close",
                "wait_process_exit.end",
                Some(process_id),
                "exited",
            ),
            Err(error) => failures.push(format!("pid={process_id}: {error:#}")),
        }
    }
    ensure!(
        failures.is_empty(),
        "并发 PTY 关闭或 child 退出确认失败: {}",
        failures.join("; ")
    );
    stage_diagnostic("concurrent-close", "scenario.end", None, "ok");
    Ok(())
}

/// 顺序创建并完整关闭 100 个真实 PTY，逐个确认 child 消失以捕获 worker 或系统句柄累积。
///
/// @author kongweiguang
async fn create_close_cycles(
    factory: &LocalPtySessionTransportFactory,
    config_root: &Path,
    total_deadline: Instant,
) -> Result<()> {
    stage_diagnostic(
        "create-close-cycle",
        "scenario.begin",
        None,
        format!("cycles={CREATE_CLOSE_CYCLE_COUNT}"),
    );
    let mut process_probes = Vec::with_capacity(CREATE_CLOSE_CYCLE_COUNT);
    for index in 0..CREATE_CLOSE_CYCLE_COUNT {
        ensure!(
            Instant::now() < total_deadline,
            "100 轮 PTY 生命周期测试启动第 {index} 轮前超过总 deadline"
        );
        let mut probe = open_owned_pty(factory, "create-close-cycle", idle_target(config_root))
            .await
            .with_context(|| format!("第 {index} 轮创建 PTY"))?;
        let process_id = probe.process_id;
        if index == 0 || (index + 1) % 10 == 0 || index + 1 == CREATE_CLOSE_CYCLE_COUNT {
            stage_diagnostic(
                "create-close-cycle",
                "cycle.open.end",
                Some(process_id),
                format!("cycle={}/{}", index + 1, CREATE_CLOSE_CYCLE_COUNT),
            );
        }
        if Instant::now() >= total_deadline {
            return return_after_owned_cleanup(
                &mut probe,
                anyhow!("100 轮 PTY 生命周期测试超过总 deadline，第 {index} 轮 pid={process_id}"),
            )
            .await;
        }
        if let Err(error) = probe
            .close_until(stage_deadline(total_deadline, CLOSE_DEADLINE))
            .await
            .with_context(|| format!("第 {index} 轮关闭 PTY，pid={process_id}"))
        {
            return return_after_owned_cleanup(&mut probe, error).await;
        }
        if index == 0 || (index + 1) % 10 == 0 || index + 1 == CREATE_CLOSE_CYCLE_COUNT {
            stage_diagnostic(
                "create-close-cycle",
                "cycle.close.end",
                Some(process_id),
                format!("cycle={}/{}", index + 1, CREATE_CLOSE_CYCLE_COUNT),
            );
        }
        process_probes.push(probe.take_process_probe()?);
    }

    let exit_deadline = stage_deadline(total_deadline, Duration::from_secs(15));
    for (index, process_probe) in process_probes.iter().enumerate() {
        let process_id = process_probe.process_id;
        if index == 0 || (index + 1) % 10 == 0 || index + 1 == CREATE_CLOSE_CYCLE_COUNT {
            stage_diagnostic(
                "create-close-cycle",
                "wait_process_exit.begin",
                Some(process_id),
                format!("cycle={}/{}", index + 1, CREATE_CLOSE_CYCLE_COUNT),
            );
        }
        wait_for_process_exit("create-close-cycle", process_probe, exit_deadline)
            .await
            .with_context(|| format!("第 {index} 轮 child 未回收，pid={process_id}"))?;
    }
    stage_diagnostic("create-close-cycle", "scenario.end", None, "ok");
    Ok(())
}

/// 按顺序运行各关闭压力场景，让失败诊断定位到具体的真实 PTY 生命周期阶段。
///
/// @author kongweiguang
async fn run_close_stress(
    factory: &LocalPtySessionTransportFactory,
    config_root: &Path,
    total_deadline: Instant,
) -> Result<()> {
    stage_diagnostic("test", "scenario.begin", None, "continuous-output");
    ensure!(
        Instant::now() < total_deadline,
        "PTY close stress 总 deadline 已过期"
    );
    stage_diagnostic(
        "test",
        "continuous_output.begin",
        None,
        "close while consuming",
    );
    close_during_consumed_output(factory, config_root, total_deadline).await?;
    stage_diagnostic("test", "continuous_output.end", None, "ok");
    stage_diagnostic("test", "backpressure.begin", None, "hold reader then close");
    close_with_output_consumer_backpressure(factory, config_root, total_deadline).await?;
    stage_diagnostic("test", "backpressure.end", None, "ok");
    stage_diagnostic("test", "repeat_close.begin", None, "idempotent close");
    close_repeatedly(factory, config_root, total_deadline).await?;
    stage_diagnostic("test", "repeat_close.end", None, "ok");
    stage_diagnostic("test", "concurrent_close.begin", None, "parallel owners");
    close_concurrently(factory, config_root, total_deadline).await?;
    stage_diagnostic("test", "concurrent_close.end", None, "ok");
    stage_diagnostic("test", "create_close_cycles.begin", None, "100 cycles");
    create_close_cycles(factory, config_root, total_deadline).await?;
    stage_diagnostic("test", "create_close_cycles.end", None, "ok");
    Ok(())
}

/// 在独立配置根与 180 秒操作 deadline 内执行 PTY 回归；外部 runner 负责拦截同步原生调用的总耗时。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_pty_close_is_bounded_under_output_backpressure_and_repetition() {
    stage_diagnostic(
        "test",
        "begin",
        None,
        "external process watchdog required; limit=180s",
    );
    let config_root = IsolatedConfigRoot::create().expect("isolated config root");
    stage_diagnostic(
        "test",
        "private_root.created",
        None,
        format!("path={:?}", config_root.path()),
    );
    let factory = LocalPtySessionTransportFactory::new();
    let deadline = Instant::now() + TEST_DEADLINE;
    let result = run_close_stress(&factory, config_root.path(), deadline).await;
    stage_diagnostic(
        "test",
        "end",
        None,
        if result.is_ok() { "ok" } else { "error" },
    );
    match result {
        Ok(()) => {}
        Err(error) => panic!(
            "local PTY close stress failed; private_root={:?}: {error:#}",
            config_root.path()
        ),
    }
}
