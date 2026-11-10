//! 文件搜索 Sidecar，依赖 serde_json 与标准库。
//! @author kongweiguang

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};

const PLUGIN_ID: &str = "kerminal.file-search";
const PLUGIN_VERSION: &str = match option_env!("KERMINAL_PLUGIN_VERSION") {
    Some(version) => version,
    None => "1.0.0",
};
const MAX_FRAME: usize = 4 * 1024 * 1024;
const MAX_RESULTS: usize = 500;
const MAX_VISITED: usize = 100_000;
const MAX_RESULT_BYTES: usize = 768 * 1024;

#[derive(Clone, Default)]
struct ViewState {
    root: String,
    query: String,
    search_id: String,
}

struct Shared {
    output: Mutex<io::Stdout>,
    pending: Mutex<HashSet<String>>,
    instances: Mutex<HashMap<String, ViewState>>,
    next_id: AtomicU64,
    locale: Mutex<String>,
}

struct Job {
    id: String,
    instance: String,
    cancelled: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for Job {
    /// 异常退出时也先请求取消并等待，避免父进程提前遗留工作子进程。
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 普通模式只通过标准输入输出收发协议，工作进程只输出搜索结果。
fn main() {
    let result = if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--worker")) {
        worker()
    } else {
        run()
    };
    if let Err(error) = result {
        eprintln!("file-search: {error}");
        std::process::exit(1);
    }
}

/// 所有输出共用一个锁和完整行，工作线程不会把两条 JSON 帧拼接。
fn emit(shared: &Shared, frame: &Value) -> Result<(), String> {
    let encoded = serde_json::to_vec(frame).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_FRAME {
        return Err("RPC frame too large".into());
    }
    let mut output = shared
        .output
        .lock()
        .map_err(|_| "output lock unavailable")?;
    output
        .write_all(&encoded)
        .map_err(|error| error.to_string())?;
    output.write_all(b"\n").map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())
}

/// 限制尚未确认的宿主请求；背压时跳过中间进度，最终动作回执仍保留结果。
fn host_request(shared: &Shared, method: &str, params: Value) -> Result<(), String> {
    let id = format!("p:{}", shared.next_id.fetch_add(1, Ordering::Relaxed));
    {
        let mut pending = shared
            .pending
            .lock()
            .map_err(|_| "pending lock unavailable")?;
        if pending.len() >= 60 {
            return Ok(());
        }
        pending.insert(id.clone());
    }
    if let Err(error) = emit(
        shared,
        &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
    ) {
        if let Ok(mut pending) = shared.pending.lock() {
            pending.remove(&id);
        }
        return Err(error);
    }
    Ok(())
}

/// 输入帧有显式上限，停止或输入关闭时等待搜索任务回收子进程。
fn run() -> Result<(), String> {
    let shared = Arc::new(Shared {
        output: Mutex::new(io::stdout()),
        pending: Mutex::new(HashSet::new()),
        instances: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        locale: Mutex::new("en".into()),
    });
    let mut initialized = false;
    let mut jobs: Vec<Job> = Vec::new();
    let stdin = io::stdin();
    let mut input = stdin.lock();
    loop {
        reap_jobs(&mut jobs);
        let mut line = Vec::new();
        let size = (&mut input)
            .take((MAX_FRAME + 2) as u64)
            .read_until(b'\n', &mut line)
            .map_err(|error| error.to_string())?;
        if size == 0 {
            break;
        }
        if line.last() != Some(&b'\n') || size > MAX_FRAME + 1 {
            return Err("invalid or oversized frame".into());
        }
        let frame: Value = match serde_json::from_slice(&line) {
            Ok(frame) => frame,
            Err(_) => {
                emit(
                    &shared,
                    &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}),
                )?;
                continue;
            }
        };
        if frame.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            continue;
        }
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            if let Some(id) = frame.get("id").and_then(Value::as_str) {
                if let Ok(mut pending) = shared.pending.lock() {
                    pending.remove(id);
                }
            }
            continue;
        };
        let id = frame.get("id").and_then(Value::as_str).map(str::to_owned);
        let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));
        if method == "$/cancel" {
            if let Some(target) = params.get("id").and_then(Value::as_str) {
                cancel_jobs(&jobs, |job| job.id == target);
            }
            continue;
        }
        if method == "initialized" || method == "view.visibility" {
            continue;
        }
        let Some(id) = id else {
            continue;
        };
        if !id.starts_with("h:") {
            continue;
        }
        if method == "initialize" {
            let identity = params.get("plugin");
            if initialized
                || params.get("protocol").and_then(Value::as_u64) != Some(1)
                || identity
                    .and_then(|value| value.get("id"))
                    .and_then(Value::as_str)
                    != Some(PLUGIN_ID)
                || identity
                    .and_then(|value| value.get("version"))
                    .and_then(Value::as_str)
                    != Some(PLUGIN_VERSION)
            {
                emit_error(&shared, &id, -32602, "invalid plugin identity")?;
                continue;
            }
            if let Some(locale) = params.get("locale").and_then(Value::as_str) {
                *shared
                    .locale
                    .lock()
                    .map_err(|_| "locale lock unavailable")? = locale.into();
            }
            initialized = true;
            emit_result(
                &shared,
                &id,
                json!({"protocol":1,"plugin":{"id":PLUGIN_ID,"version":PLUGIN_VERSION}}),
            )?;
            continue;
        }
        if !initialized {
            emit_error(&shared, &id, -32600, "not initialized")?;
            continue;
        }
        match method {
            "$/ping" => emit_result(&shared, &id, json!({}))?,
            "shutdown" => {
                cancel_jobs(&jobs, |_| true);
                join_jobs(&mut jobs);
                emit_result(&shared, &id, json!({}))?;
                break;
            }
            "view.open" => open_view(&shared, &id, &params)?,
            "view.snapshot" | "view.close" => {
                snapshot(&shared, &id, &params, method == "view.close", &mut jobs)?
            }
            "view.action" => action(&shared, &id, &params, &mut jobs)?,
            _ => emit_error(&shared, &id, -32601, "method not found")?,
        }
    }
    cancel_jobs(&jobs, |_| true);
    join_jobs(&mut jobs);
    Ok(())
}

/// 统一回执格式，业务结果与传输错误不能同时出现。
fn emit_result(shared: &Shared, id: &str, result: Value) -> Result<(), String> {
    emit(shared, &json!({"jsonrpc":"2.0","id":id,"result":result}))
}

/// 错误只包含固定说明，不回显用户路径或文件内容。
fn emit_error(shared: &Shared, id: &str, code: i32, message: &str) -> Result<(), String> {
    emit(
        shared,
        &json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}),
    )
}

/// 工作区状态只恢复路径和查询，崩溃后的旧结果不得冒充新搜索结果。
fn open_view(shared: &Shared, id: &str, params: &Value) -> Result<(), String> {
    let Some(instance) = params.get("instance").and_then(Value::as_str) else {
        return emit_error(shared, id, -32602, "instance required");
    };
    let saved = params.get("state").unwrap_or(&Value::Null);
    let view = ViewState {
        root: bounded(saved.get("root"), 4096),
        query: bounded(saved.get("query"), 256),
        search_id: String::new(),
    };
    shared
        .instances
        .lock()
        .map_err(|_| "instances lock unavailable")?
        .insert(instance.into(), view.clone());
    emit_result(
        shared,
        id,
        json!({"data":{"root":view.root,"query":view.query,"results":[],"busy":false}}),
    )
}

/// 关闭先使实例失效，再释放实例锁并等待所属任务回收；成功回执不能先于工作进程退出。
fn snapshot(
    shared: &Shared,
    id: &str,
    params: &Value,
    close: bool,
    jobs: &mut Vec<Job>,
) -> Result<(), String> {
    let Some(instance) = params.get("instance").and_then(Value::as_str) else {
        return emit_error(shared, id, -32602, "instance required");
    };
    let mut instances = shared
        .instances
        .lock()
        .map_err(|_| "instances lock unavailable")?;
    let Some(view) = instances.get(instance).cloned() else {
        return emit_error(shared, id, -32004, "stale instance");
    };
    if close {
        instances.remove(instance);
    }
    drop(instances);
    if close {
        cancel_jobs(jobs, |job| job.instance == instance);
        // 删除 Job 会 join；此时不再持有 instances 锁，工作线程可正常检查失效状态并退出。
        jobs.retain(|job| job.instance != instance);
    }
    emit_result(
        shared,
        id,
        json!({"state_version":1,"state":{"root":view.root,"query":view.query}}),
    )
}

/// 输入值超限时截断到 UTF-8 字符边界，避免扩大快照和请求帧。
fn bounded(value: Option<&Value>, limit: usize) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(limit)
        .collect()
}

/// 同一视图只保留最新搜索；草稿更新不启动工作进程。
fn action(
    shared: &Arc<Shared>,
    id: &str,
    params: &Value,
    jobs: &mut Vec<Job>,
) -> Result<(), String> {
    let Some(instance) = params.get("instance").and_then(Value::as_str) else {
        return emit_error(shared, id, -32602, "instance required");
    };
    let data = params.get("data").unwrap_or(&Value::Null);
    let mut instances = shared
        .instances
        .lock()
        .map_err(|_| "instances lock unavailable")?;
    let Some(view) = instances.get_mut(instance) else {
        return emit_error(shared, id, -32004, "stale instance");
    };
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if action != "draft" && action != "search" {
        return emit_error(shared, id, -32601, "unknown action");
    }
    view.root = bounded(data.get("root"), 4096);
    view.query = bounded(data.get("query"), 256);
    if action == "draft" {
        drop(instances);
        return emit_result(shared, id, json!({}));
    }
    if view.root.trim().is_empty() || view.query.trim().is_empty() {
        return emit_error(shared, id, -32602, "root and query required");
    }
    if jobs.iter().any(|job| job.id == id) || jobs.len() >= 64 {
        return emit_error(shared, id, -32003, "request unavailable");
    }
    cancel_jobs(jobs, |job| job.instance == instance);
    view.search_id = id.into();
    let view = view.clone();
    drop(instances);
    let cancelled = Arc::new(AtomicBool::new(false));
    let task_cancelled = Arc::clone(&cancelled);
    let task_shared = Arc::clone(shared);
    let task_id = id.to_owned();
    let task_instance = instance.to_owned();
    let handle =
        thread::spawn(move || search(task_shared, task_id, task_instance, view, task_cancelled));
    jobs.push(Job {
        id: id.into(),
        instance: instance.into(),
        cancelled,
        handle: Some(handle),
    });
    Ok(())
}

/// 取消只设置标记；拥有 Child 的线程负责 kill 和 wait，确保没有悬挂进程。
fn cancel_jobs<F: Fn(&Job) -> bool>(jobs: &[Job], predicate: F) {
    for job in jobs.iter().filter(|job| predicate(job)) {
        job.cancelled.store(true, Ordering::Release);
    }
}

/// 已结束任务及时回收句柄，避免反复搜索累积线程资源。
fn reap_jobs(jobs: &mut Vec<Job>) {
    let mut index = 0;
    while index < jobs.len() {
        if jobs[index]
            .handle
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            drop(jobs.swap_remove(index));
        } else {
            index += 1;
        }
    }
}

/// 进程退出前等待所有任务；每个任务都会回收其直接子进程。
fn join_jobs(jobs: &mut Vec<Job>) {
    jobs.clear();
}

/// 新搜索或关闭实例会使旧工作线程失效，防止迟到结果覆盖当前视图。
fn current(shared: &Shared, instance: &str, id: &str) -> bool {
    shared
        .instances
        .lock()
        .ok()
        .and_then(|instances| instances.get(instance).map(|view| view.search_id == id))
        .unwrap_or(false)
}

/// Sidecar 启动独立工作进程；stdin 主循环保持可取消，所有退出路径都等待子进程。
fn search(
    shared: Arc<Shared>,
    id: String,
    instance: String,
    view: ViewState,
    cancelled: Arc<AtomicBool>,
) {
    let executable = match env::current_exe() {
        Ok(path) => path,
        Err(_) => {
            let _ = emit_error(&shared, &id, -32100, "worker unavailable");
            return;
        }
    };
    let mut command = Command::new(executable);
    command
        .arg("--worker")
        .arg(&view.root)
        .arg(&view.query)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            let _ = emit_error(&shared, &id, -32100, "worker start failed");
            return;
        }
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = emit_error(&shared, &id, -32100, "worker output unavailable");
        return;
    };
    let (sender, receiver) = mpsc::sync_channel::<String>(64);
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if sender.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let mut rows = Vec::<Value>::new();
    let mut row_bytes = 0usize;
    let mut failure = None;
    let mut limited = false;
    let mut skipped = 0u64;
    let locale = shared
        .locale
        .lock()
        .map(|value| value.clone())
        .unwrap_or_else(|_| "en".into());
    let progress = if locale.to_lowercase().starts_with("zh") {
        "正在搜索文件"
    } else {
        "Searching files"
    };
    let _ = host_request(
        &shared,
        "$/progress",
        json!({"token":id,"message":progress}),
    );
    loop {
        if cancelled.load(Ordering::Acquire) || !current(&shared, &instance, &id) {
            let _ = child.kill();
            break;
        }
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    if let Some(path) = value.get("path").and_then(Value::as_str) {
                        let row = json!({"path":path});
                        let size = serde_json::to_vec(&row)
                            .map_or(MAX_RESULT_BYTES, |bytes| bytes.len() + 1);
                        if row_bytes.saturating_add(size) > MAX_RESULT_BYTES {
                            limited = true;
                            continue;
                        }
                        row_bytes += size;
                        rows.push(row);
                        if rows.len() % 20 == 0 {
                            let _ = host_request(
                                &shared,
                                "view.update",
                                json!({"instance":instance,"patch":{"results":rows,"busy":true}}),
                            );
                        }
                    } else if let Some(summary) = value.get("summary") {
                        limited |= summary
                            .get("limited")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        skipped = summary.get("skipped").and_then(Value::as_u64).unwrap_or(0);
                    } else if value.get("error").is_some() {
                        failure = Some("search failed");
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(receiver);
    let status = child.wait();
    let _ = reader.join();
    if cancelled.load(Ordering::Acquire) || !current(&shared, &instance, &id) {
        let _ = emit_error(&shared, &id, -32002, "cancelled");
    } else if failure.is_some() || !status.is_ok_and(|status| status.success()) {
        let _ = emit_error(&shared, &id, -32100, "search failed");
    } else {
        let patch = json!({"results":rows,"busy":false,"limited":limited,"skipped":skipped});
        let _ = host_request(
            &shared,
            "view.update",
            json!({"instance":instance,"patch":patch}),
        );
        let _ = emit_result(&shared, &id, json!({"patch":patch}));
    }
}

/// 工作进程不跟随符号链接，并限制遍历和结果数量，避免意外扫描无限目录树。
fn worker() -> Result<(), String> {
    let mut arguments = env::args_os().skip(2);
    let root = PathBuf::from(arguments.next().ok_or("root required")?);
    let query = arguments
        .next()
        .ok_or("query required")?
        .to_string_lossy()
        .to_lowercase();
    if arguments.next().is_some() || query.is_empty() || !root.is_dir() {
        return Err("invalid search arguments".into());
    }
    let mut pending = vec![root];
    let mut visited = 0usize;
    let mut matched = 0usize;
    let mut errors = 0usize;
    let mut limited = false;
    let mut output = io::BufWriter::new(io::stdout().lock());
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    errors += 1;
                    continue;
                }
            };
            visited += 1;
            if visited > MAX_VISITED {
                limited = true;
                break;
            }
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(_) => {
                    errors += 1;
                    continue;
                }
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path.clone());
            }
            if kind.is_file()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .to_lowercase()
                    .contains(&query)
            {
                serde_json::to_writer(&mut output, &json!({"path":path.to_string_lossy()}))
                    .map_err(|error| error.to_string())?;
                writeln!(output).map_err(|error| error.to_string())?;
                output.flush().map_err(|error| error.to_string())?;
                matched += 1;
                if matched >= MAX_RESULTS {
                    limited = true;
                    break;
                }
            }
        }
        if visited > MAX_VISITED || matched >= MAX_RESULTS {
            break;
        }
    }
    serde_json::to_writer(
        &mut output,
        &json!({"summary":{"limited":limited,"skipped":errors}}),
    )
    .map_err(|error| error.to_string())?;
    writeln!(output).map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())
}
