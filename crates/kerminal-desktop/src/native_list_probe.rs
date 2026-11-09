//! P0.4 专用窗口和隔离 JSON-RPC 夹具；由 xtask 在私有桌面启动。
//!
//! @author kongweiguang

mod protocol;

use std::{
    env,
    error::Error,
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use gpui_kit::{AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};
use kerminal_ui::plugins::native_probe::{
    NativeListProbe, PageCache, PageRequest, PageResponse, ProbeResult, ProbeRow, ROW_COUNT,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const WORKER_ARGUMENT: &str = "--native-list-probe-worker";
const WORKER_TIMEOUT: Duration = Duration::from_secs(5);
const WINDOW_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_RPC_FRAME_BYTES: usize = 1024 * 1024;

/// 验收只接受显式输出路径和隔离配置根，避免触碰用户的实际配置。
pub(crate) fn run() -> Result<(), Box<dyn Error>> {
    let output = output_path(env::args_os().skip(2))?;
    if env::var_os("KERMINAL_CONFIG_ROOT").is_none_or(|value| value.is_empty()) {
        return Err(io::Error::other("native list probe requires KERMINAL_CONFIG_ROOT").into());
    }
    let environment = build_environment()?;
    let mut worker = ProbeWorker::start()?;
    let initial_request = PageRequest::new(0, 0..64);
    worker.requests.send(initial_request)?;
    let receiver = worker
        .responses
        .take()
        .ok_or_else(|| io::Error::other("RPC receiver missing"))?;
    let initial = receiver
        .recv_timeout(WORKER_TIMEOUT)
        .map_err(|error| io::Error::other(format!("initial RPC did not complete: {error}")))?
        .map_err(io::Error::other)?;
    let cache = PageCache::new(ROW_COUNT, initial).map_err(io::Error::other)?;
    let result: ProbeResult = Arc::new(Mutex::new(None));
    let window_result = run_window(
        cache,
        worker.requests.clone(),
        receiver,
        Arc::clone(&result),
    );
    let mut report = result.lock()
        .map_err(|_| io::Error::other("probe result lock poisoned"))?
        .take()
        .unwrap_or_else(|| json!({"schema": 1, "verdict": "inconclusive", "error": "window exited without a report"}));
    if let Err(error) = window_result {
        report["verdict"] = json!("inconclusive");
        report["error"] = json!(error);
    }
    report["build_and_hardware"] = environment;
    let verdict = report["verdict"]
        .as_str()
        .unwrap_or("inconclusive")
        .to_owned();
    let mut file = File::options().write(true).create_new(true).open(&output)?;
    serde_json::to_writer_pretty(&mut file, &report)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    println!("Native list probe report: {}", output.display());
    if verdict != "passed" {
        return Err(io::Error::other(format!("native list probe: {verdict}")).into());
    }
    Ok(())
}

/// 子进程仅服务受控 `view.fetch`，标准输出保持纯 NDJSON 协议。
pub(crate) fn run_worker() -> Result<(), Box<dyn Error>> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if line.len() > MAX_RPC_FRAME_BYTES {
            return Err(io::Error::other("request exceeds frame limit").into());
        }
        let request: Value = serde_json::from_str(&line)?;
        let response = protocol::handle(&request).map_err(io::Error::other)?;
        serde_json::to_writer(&mut output, &response)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
}

/// 只接受一个尚不存在的输出文件；xtask 负责建立验收目录。
fn output_path(arguments: impl Iterator<Item = std::ffi::OsString>) -> io::Result<PathBuf> {
    let mut arguments = arguments;
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--output")) {
        return Err(io::Error::other("expected --output <new-report.json>"));
    }
    let output = arguments
        .next()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("missing output path"))?;
    if arguments.next().is_some() {
        return Err(io::Error::other("unexpected native list probe argument"));
    }
    Ok(output)
}

/// 在进入 UI 循环之前读取二进制摘要和系统信息，不在渲染线程执行磁盘操作。
fn build_environment() -> io::Result<Value> {
    let executable = env::current_exe()?;
    let mut file = File::open(&executable)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let cpu = env::var("PROCESSOR_IDENTIFIER")
        .ok()
        .or_else(linux_cpu_name);
    Ok(json!({
        "os": env::consts::OS,
        "architecture": env::consts::ARCH,
        "cpu_identifier": cpu,
        "logical_cpu_count": thread::available_parallelism().ok().map(usize::from),
        "executable": executable,
        "executable_sha256": format!("{:x}", digest.finalize()),
        "package_version": env!("CARGO_PKG_VERSION"),
        "debug_assertions": cfg!(debug_assertions),
    }))
}

/// Linux 没有统一的处理器环境变量；读取失败时明确保留未知值。
fn linux_cpu_name() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let contents = std::fs::read_to_string("/proc/cpuinfo").ok()?;
        return contents.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            matches!(key.trim(), "model name" | "Hardware").then(|| value.trim().to_owned())
        });
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// 指定独立的真实窗口和 watchdog；任何无帧退出都记录为证据不足。
fn run_window(
    cache: PageCache,
    requests: mpsc::Sender<PageRequest>,
    responses: mpsc::Receiver<Result<PageResponse, String>>,
    result: ProbeResult,
) -> Result<(), String> {
    gpui_kit::profiler::set_trace_enabled(true);
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let outcome_for_app = Arc::clone(&result);
        gpui_kit::application()
            .with_assets(gpui_kit::assets::AllAssets)
            .run(move |cx| {
                kerminal_ui::init(cx);
                let options = WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(1000.0), px(720.0)),
                        cx,
                    ))),
                    show: true,
                    focus: true,
                    ..Default::default()
                };
                let window_result = Arc::clone(&outcome_for_app);
                let opened = gpui_kit::open_window(options, cx, move |window, cx| {
                    cx.new(|cx| {
                        NativeListProbe::new(cache, requests, responses, window_result, window, cx)
                    })
                });
                if let Err(error) = opened {
                    record_failure(
                        &outcome_for_app,
                        format!("opening native window failed: {error}"),
                    );
                    cx.quit();
                    return;
                }
                let timeout_result = Arc::clone(&outcome_for_app);
                cx.spawn(async move |cx| {
                    cx.background_executor().timer(WINDOW_TIMEOUT).await;
                    record_failure(
                        &timeout_result,
                        "native window timed out before completing frames".into(),
                    );
                    cx.update(|cx| cx.quit());
                })
                .detach();
            });
    }));
    gpui_kit::profiler::set_trace_enabled(false);
    outcome.map_err(|_| "native window event loop panicked".to_owned())
}

/// 失败只写首次结果，防止 watchdog 覆盖已完成的真实样本。
fn record_failure(result: &ProbeResult, error: String) {
    if let Ok(mut result) = result.lock() {
        if result.is_none() {
            *result = Some(json!({"schema": 1, "verdict": "inconclusive", "error": error}));
        }
    }
}

/// 进程和其唯一 I/O 线程由组合根持有，UI 只持有有界工作量的请求通道。
struct ProbeWorker {
    child: Child,
    requests: mpsc::Sender<PageRequest>,
    responses: Option<mpsc::Receiver<Result<PageResponse, String>>>,
    thread: Option<JoinHandle<()>>,
}

impl ProbeWorker {
    /// 以相同二进制的私有模式启动夹具，不依赖外部插件或可见终端。
    fn start() -> io::Result<Self> {
        let mut command = Command::new(env::current_exe()?);
        command
            .arg(WORKER_ARGUMENT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let child = command.spawn()?;
        let (requests, request_receiver) = mpsc::channel();
        let (response_sender, responses) = mpsc::channel();
        // 先登记进程所有权，后续管道或线程初始化失败也会进入 Drop 回收。
        let mut worker = Self {
            child,
            requests,
            responses: Some(responses),
            thread: None,
        };
        let input = worker
            .child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("worker stdin missing"))?;
        let output = worker
            .child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("worker stdout missing"))?;
        let thread = thread::Builder::new()
            .name("native-probe-rpc".into())
            .spawn(move || {
                let mut input = input;
                let mut output = BufReader::new(output);
                for request in request_receiver {
                    if request.id() == u64::MAX {
                        break;
                    }
                    let response = exchange(&mut input, &mut output, &request);
                    let failed = response.is_err();
                    if response_sender.send(response).is_err() || failed {
                        break;
                    }
                }
            })?;
        worker.thread = Some(thread);
        Ok(worker)
    }
}

impl Drop for ProbeWorker {
    /// 收尾先关闭管道并等子进程退出；超时强制回收，避免遗留夹具进程。
    fn drop(&mut self) {
        if self.thread.is_none() {
            let _ = self.child.stdin.take();
            let _ = self.child.stdout.take();
            let _ = self.child.kill();
            let _ = self.child.wait();
            return;
        }
        let _ = self.requests.send(PageRequest::new(u64::MAX, 0..0));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 对照请求 id、范围和逐行字段解码回应，错误通过通道返回 UI 而非伪造空页。
fn exchange(
    input: &mut ChildStdin,
    output: &mut BufReader<ChildStdout>,
    request: &PageRequest,
) -> Result<PageResponse, String> {
    let range = request.range();
    let message = json!({"jsonrpc":"2.0", "id":request.id(), "method":"view.fetch", "params": {
        "view":"native-probe", "instance":"native-probe-instance", "node":"rows", "range":[range.start,range.end]
    }});
    serde_json::to_writer(&mut *input, &message).map_err(|error| error.to_string())?;
    input
        .write_all(b"\n")
        .and_then(|_| input.flush())
        .map_err(|error| error.to_string())?;
    let mut line = String::new();
    if output
        .read_line(&mut line)
        .map_err(|error| error.to_string())?
        == 0
    {
        return Err("worker closed RPC stream".into());
    }
    if line.len() > MAX_RPC_FRAME_BYTES {
        return Err("RPC response exceeds frame limit".into());
    }
    let response: Value = serde_json::from_str(&line).map_err(|error| error.to_string())?;
    if response["jsonrpc"] != "2.0"
        || response["id"].as_u64() != Some(request.id())
        || response["result"]["range"] != json!([range.start, range.end])
    {
        return Err("RPC response identity mismatch".into());
    }
    let rows = response["result"]["rows"]
        .as_array()
        .ok_or("RPC rows missing")?
        .iter()
        .map(|row| {
            let index = usize::try_from(row["index"].as_u64().ok_or("RPC row index missing")?)
                .map_err(|_| "RPC row index overflow")?;
            let title = row["title"]
                .as_str()
                .ok_or("RPC row title missing")?
                .to_owned();
            let subtitle = row["subtitle"]
                .as_str()
                .ok_or("RPC row subtitle missing")?
                .to_owned();
            Ok(ProbeRow::new(index, title, subtitle))
        })
        .collect::<Result<Vec<_>, &str>>()
        .map_err(str::to_owned)?;
    Ok(PageResponse::new(request.clone(), rows))
}
