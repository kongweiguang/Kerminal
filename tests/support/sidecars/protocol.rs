// 验收 Sidecar 共用的 JSON-RPC v1 主循环与双向请求骨架。
// @author kongweiguang
//
// 测试夹具需要真实进程边界，但不应各自复制协议细节；这里只保留宿主使用的
// 握手、取消、回执和退出语义，具体业务由各夹具定义 `dispatch` 与通知入口。

use std::{
    collections::{HashMap, HashSet},
    io::{self, BufRead, Read, Write},
    process,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use serde_json::{Value, json};

const PROTOCOL_VERSION: u64 = 1;
const MAX_FRAME: usize = 4 * 1024 * 1024;
const MAX_PENDING: usize = 64;

type RpcError = (i32, String);
type RpcResult = Result<Value, RpcError>;

struct Shared {
    output: Mutex<io::Stdout>,
    pending: Mutex<HashMap<String, mpsc::Sender<Result<Value, RpcError>>>>,
    next: AtomicU64,
    locale: Mutex<String>,
    active: Mutex<HashSet<String>>,
    cancelled: Mutex<HashSet<String>>,
}

/// 每个业务回调携带自身请求身份，宿主的取消只影响对应动作而不是整个插件。
#[derive(Clone)]
struct Ctx {
    shared: Arc<Shared>,
    request: String,
}

impl Ctx {
    /// 宿主调用必须等到回执才能更新计数；等待期间持续检查取消，避免关闭后继续写视图。
    fn call(&self, method: &str, params: Value) -> RpcResult {
        let id = format!("p:{}", self.shared.next.fetch_add(1, Ordering::Relaxed));
        let (sender, receiver) = mpsc::channel();
        self.shared
            .pending
            .lock()
            .map_err(|_| internal("pending lock unavailable"))?
            .insert(id.clone(), sender);
        emit(
            &self.shared,
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )?;
        loop {
            if self.cancelled() {
                if let Ok(mut pending) = self.shared.pending.lock() {
                    pending.remove(&id);
                }
                let _ = emit(
                    &self.shared,
                    &json!({"jsonrpc":"2.0","method":"$/cancel","params":{"id":id}}),
                );
                return Err((-32002, "cancelled".into()));
            }
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(internal("connection closed"));
                }
            }
        }
    }

    /// 取消状态按请求 ID 保存，同一进程中的其他视图不受影响。
    fn cancelled(&self) -> bool {
        !self.request.is_empty()
            && self
                .shared
                .cancelled
                .lock()
                .map(|cancelled| cancelled.contains(&self.request))
                .unwrap_or(true)
    }

    /// 夹具文案与宿主界面语言保持同源，非中文统一回退英文。
    fn chinese(&self) -> bool {
        self.shared
            .locale
            .lock()
            .map(|locale| locale.to_lowercase().starts_with("zh"))
            .unwrap_or(false)
    }
}

/// 协议错误只写标准错误，标准输出始终只承载完整 NDJSON 帧。
fn main() {
    if let Err(error) = run() {
        eprintln!("{}: {error}", plugin::PLUGIN_ID);
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let shared = Arc::new(Shared {
        output: Mutex::new(io::stdout()),
        pending: Mutex::new(HashMap::new()),
        next: AtomicU64::new(1),
        locale: Mutex::new("en".into()),
        active: Mutex::new(HashSet::new()),
        cancelled: Mutex::new(HashSet::new()),
    });
    let mut initialized = false;
    let mut jobs: Vec<JoinHandle<()>> = Vec::new();
    let mut notifications: Vec<JoinHandle<()>> = Vec::new();
    let stdin = io::stdin();
    let mut input = stdin.lock();
    loop {
        reap(&mut jobs);
        reap(&mut notifications);
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
                )
                .map_err(|(_, message)| message)?;
                continue;
            }
        };
        if frame.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            continue;
        }
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            handle_response(&shared, &frame);
            continue;
        };
        let method = method.to_owned();
        let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));
        if method == "$/cancel" {
            if let Some(target) = params.get("id").and_then(Value::as_str)
                && let Ok(mut cancelled) = shared.cancelled.lock()
            {
                cancelled.insert(target.into());
            }
            continue;
        }
        let Some(id) = frame.get("id").and_then(Value::as_str).map(str::to_owned) else {
            // 设置变更等通知不占用请求槽；失败只留在标准错误，不能伪造成功回执。
            if initialized {
                let ctx = Ctx {
                    shared: Arc::clone(&shared),
                    request: String::new(),
                };
                let method = method.clone();
                let params = params.clone();
                notifications.push(thread::spawn(move || {
                    if let Err((_, message)) = plugin::on_notification(&ctx, &method, params) {
                        eprintln!("{}: notification failed: {message}", plugin::PLUGIN_ID);
                    }
                }));
            }
            continue;
        };
        if method == "initialize" {
            if initialized {
                let _ = emit_error(&shared, &id, -32600, "already initialized");
                continue;
            }
            if !valid_identity(&params) {
                let _ = emit_error(&shared, &id, -32602, "invalid plugin identity");
                continue;
            }
            if let Some(locale) = params.get("locale").and_then(Value::as_str)
                && let Ok(mut current) = shared.locale.lock()
            {
                *current = locale.into();
            }
            initialized = true;
            let _ = emit_result(
                &shared,
                &id,
                json!({"protocol":PROTOCOL_VERSION,"plugin":{"id":plugin::PLUGIN_ID,"version":plugin::PLUGIN_VERSION}}),
            );
            continue;
        }
        if !initialized {
            let _ = emit_error(&shared, &id, -32600, "not initialized");
            continue;
        }
        if method == "$/ping" {
            let _ = emit_result(&shared, &id, json!({}));
            continue;
        }
        if method == "shutdown" {
            plugin::on_shutdown(&shared);
            cancel_all(&shared);
            join_all(&mut jobs);
            join_all(&mut notifications);
            let _ = emit_result(&shared, &id, json!({}));
            break;
        }
        if !id.starts_with("h:") {
            continue;
        }
        {
            let mut active = shared
                .active
                .lock()
                .map_err(|_| "active lock unavailable")?;
            if active.contains(&id) {
                drop(active);
                let _ = emit_error(&shared, &id, -32600, "duplicate request");
                continue;
            }
            if active.len() >= MAX_PENDING {
                drop(active);
                let _ = emit_error(&shared, &id, -32003, "overloaded");
                continue;
            }
            active.insert(id.clone());
        }
        let ctx = Ctx {
            shared: Arc::clone(&shared),
            request: id.clone(),
        };
        let frame_id = id.clone();
        jobs.push(thread::spawn(move || {
            let result = plugin::dispatch(&ctx, &method, params);
            let cancelled = ctx.cancelled();
            if let Ok(mut active) = ctx.shared.active.lock() {
                active.remove(&frame_id);
            }
            if let Ok(mut pending) = ctx.shared.cancelled.lock() {
                pending.remove(&frame_id);
            }
            let frame = if cancelled {
                json!({"jsonrpc":"2.0","id":frame_id,"error":{"code":-32002,"message":"cancelled"}})
            } else {
                match result {
                    Ok(value) => json!({"jsonrpc":"2.0","id":frame_id,"result":value}),
                    Err((code, message)) => {
                        json!({"jsonrpc":"2.0","id":frame_id,"error":{"code":code,"message":message}})
                    }
                }
            };
            let _ = emit(&ctx.shared, &frame);
        }));
    }
    plugin::on_shutdown(&shared);
    cancel_all(&shared);
    join_all(&mut jobs);
    join_all(&mut notifications);
    Ok(())
}

/// 身份回显必须与编译期清单身份一致，不能把任意可执行文件挂到宿主声明的插件上。
fn valid_identity(params: &Value) -> bool {
    params
        .get("plugin")
        .is_some_and(|plugin| {
            plugin.get("id").and_then(Value::as_str) == Some(plugin::PLUGIN_ID)
                && plugin.get("version").and_then(Value::as_str) == Some(plugin::PLUGIN_VERSION)
        })
        && params.get("protocol").and_then(Value::as_u64) == Some(PROTOCOL_VERSION)
}

/// 所有输出共用一个锁和完整行，工作线程不会把两条 JSON 帧拼接。
fn emit(shared: &Shared, frame: &Value) -> Result<(), RpcError> {
    let mut bytes = serde_json::to_vec(frame).map_err(|_| internal("encode failed"))?;
    if bytes.len() + 1 > MAX_FRAME {
        return Err((-32100, "frame too large".into()));
    }
    bytes.push(b'\n');
    let mut output = shared
        .output
        .lock()
        .map_err(|_| internal("output lock unavailable"))?;
    output
        .write_all(&bytes)
        .and_then(|_| output.flush())
        .map_err(|_| internal("output write failed"))
}

fn emit_result(shared: &Shared, id: &str, result: Value) -> Result<(), RpcError> {
    emit(shared, &json!({"jsonrpc":"2.0","id":id,"result":result}))
}

fn emit_error(shared: &Shared, id: &str, code: i32, message: &str) -> Result<(), RpcError> {
    emit(
        shared,
        &json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}),
    )
}

/// 宿主回执只结算对应请求；迟到或未知 ID 不会影响其他视图。
fn handle_response(shared: &Shared, frame: &Value) {
    let Some(id) = frame.get("id").and_then(Value::as_str) else {
        return;
    };
    let Some(sender) = shared
        .pending
        .lock()
        .ok()
        .and_then(|mut pending| pending.remove(id))
    else {
        return;
    };
    let result = match frame.get("error") {
        Some(error) => Err((
            error
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or(-32100) as i32,
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("host error")
                .into(),
        )),
        None => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
    };
    let _ = sender.send(result);
}

/// 关闭与 EOF 都先取消全部请求，再等待线程回收，避免进程提前退出留下在途动作。
fn cancel_all(shared: &Shared) {
    if let Ok(mut cancelled) = shared.cancelled.lock()
        && let Ok(active) = shared.active.lock()
    {
        cancelled.extend(active.iter().cloned());
    }
}

/// 已结束线程及时回收，避免长期运行持续累积句柄。
fn reap(jobs: &mut Vec<JoinHandle<()>>) {
    let mut index = 0;
    while index < jobs.len() {
        if jobs[index].is_finished() {
            let handle = jobs.swap_remove(index);
            let _ = handle.join();
        } else {
            index += 1;
        }
    }
}

fn join_all(jobs: &mut Vec<JoinHandle<()>>) {
    for handle in jobs.drain(..) {
        let _ = handle.join();
    }
}

fn internal(message: &str) -> RpcError {
    (-32100, message.into())
}

/// 字符串参数缺失时返回类型化协议错误，避免把空实例当成合法身份。
fn string_param(params: &Value, name: &str) -> Result<String, RpcError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or((-32602, format!("{name} required")))
}