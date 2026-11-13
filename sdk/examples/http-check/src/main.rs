//! 官方主机类型示例的 Rust Sidecar：检查 example.com 并维护按实例隔离的结果标签。
//! @author kongweiguang
//!
//! 插件启用后即被完全信任：Sidecar 以当前用户权限运行，网络请求由 Sidecar 直接
//! 完成；清单不声明也不要求网络权限，实际连通范围受操作系统网络机制约束。

use std::{
    collections::{HashMap, HashSet},
    io::{self, BufRead, Read, Write},
    process,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use reqwest::{blocking::Client, redirect::Policy};
use serde_json::{Value, json};

const PLUGIN_ID: &str = "kerminal.http-check";
const PLUGIN_VERSION: &str = match option_env!("KERMINAL_PLUGIN_VERSION") {
    Some(version) => version,
    None => "1.0.0",
};
const PROTOCOL_VERSION: u64 = 1;
const MAX_FRAME: usize = 4 * 1024 * 1024;
const MAX_PENDING: usize = 64;
const DEFAULT_URL: &str = "https://example.com/";

type RpcError = (i32, String);
type RpcResult = Result<Value, RpcError>;

#[derive(Clone)]
struct ResultState {
    url: String,
    message: String,
    ok: bool,
    generation: u64,
}

struct Shared {
    output: Mutex<io::Stdout>,
    locale: Mutex<String>,
    instances: Mutex<HashMap<String, ResultState>>,
    active: Mutex<HashSet<String>>,
    cancelled: Mutex<HashSet<String>>,
    next_generation: AtomicU64,
}

#[derive(Clone)]
struct Ctx {
    shared: Arc<Shared>,
    request: String,
}

impl Ctx {
    /// 取消只作用于当前请求；网络调用有 10 秒上限，最迟在该上限后释放线程。
    fn cancelled(&self) -> bool {
        !self.request.is_empty()
            && self
                .shared
                .cancelled
                .lock()
                .map(|cancelled| cancelled.contains(&self.request))
                .unwrap_or(true)
    }

    /// 握手语言决定运行时文案；非中文统一回退英文。
    fn chinese(&self) -> bool {
        self.shared
            .locale
            .lock()
            .map(|locale| locale.to_lowercase().starts_with("zh"))
            .unwrap_or(false)
    }
}

/// 普通模式只通过标准输入输出收发协议帧，诊断只写标准错误。
fn main() {
    if let Err(error) = run() {
        eprintln!("http-check: {error}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let shared = Arc::new(Shared {
        output: Mutex::new(io::stdout()),
        locale: Mutex::new("en".into()),
        instances: Mutex::new(HashMap::new()),
        active: Mutex::new(HashSet::new()),
        cancelled: Mutex::new(HashSet::new()),
        next_generation: AtomicU64::new(1),
    });
    let mut initialized = false;
    let mut jobs: Vec<JoinHandle<()>> = Vec::new();
    let stdin = io::stdin();
    let mut input = stdin.lock();
    loop {
        reap(&mut jobs);
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
            // 本示例不向宿主发起请求；迟到的回执没有等待者，忽略即可。
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
            // view.visibility 等通知没有业务副作用，初始化前也不能扩大能力。
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
                json!({"protocol":PROTOCOL_VERSION,"plugin":{"id":PLUGIN_ID,"version":PLUGIN_VERSION}}),
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
            cancel_all(&shared);
            join_all(&mut jobs);
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
            let result = dispatch(&ctx, &method, params);
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
    cancel_all(&shared);
    join_all(&mut jobs);
    Ok(())
}

/// 业务分派只接受协议列出的方法；未知方法返回类型化错误而不是静默成功。
fn dispatch(ctx: &Ctx, method: &str, params: Value) -> RpcResult {
    match method {
        "host.test" => test_host(ctx, &params),
        "host.open" => open_host(ctx, &params),
        "view.open" => open_view(ctx, &params),
        "view.action" => action(ctx, &params),
        "view.snapshot" => snapshot(ctx, &params, false),
        "view.close" => snapshot(ctx, &params, true),
        _ => Err((-32601, "method not found".into())),
    }
}

/// 测试连接只接受明确允许的地址，网络失败不伪装成成功。
fn test_host(ctx: &Ctx, params: &Value) -> RpcResult {
    let result = match endpoint(ctx, &params.get("config").cloned().unwrap_or(Value::Null)) {
        Ok(url) => probe(ctx, &url),
        Err(message) => ResultState {
            url: DEFAULT_URL.into(),
            message,
            ok: false,
            generation: 0,
        },
    };
    Ok(json!({"ok":result.ok,"message":result.message}))
}

/// 打开连接时先取得实际结果，标签状态包含原地址与结果供重启恢复。
fn open_host(ctx: &Ctx, params: &Value) -> RpcResult {
    let url = endpoint(ctx, &params.get("config").cloned().unwrap_or(Value::Null))
        .map_err(|message| (-32100, message))?;
    let state = probe(ctx, &url);
    Ok(json!({"tab":"http-result","state":state_json(&state)}))
}

/// 恢复缺损状态时只给默认地址，避免使用未验证的任意网络目标。
fn open_view(ctx: &Ctx, params: &Value) -> RpcResult {
    let instance = string_param(params, "instance")?;
    let saved = params.get("state").cloned().unwrap_or(Value::Null);
    let url = saved
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| allowed(url))
        .unwrap_or(DEFAULT_URL)
        .to_owned();
    let state = ResultState {
        url,
        message: saved
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        ok: saved.get("ok").and_then(Value::as_bool).unwrap_or(false),
        generation: ctx.shared.next_generation.fetch_add(1, Ordering::Relaxed),
    };
    ctx.shared
        .instances
        .lock()
        .map_err(|_| internal("instances lock unavailable"))?
        .insert(instance, state.clone());
    Ok(json!({"data":state_json(&state)}))
}

/// 重查使用该实例自己的地址；检查期间重新打开或再次重查会让本次结果失效。
fn action(ctx: &Ctx, params: &Value) -> RpcResult {
    let instance = string_param(params, "instance")?;
    let current = {
        let instances = ctx
            .shared
            .instances
            .lock()
            .map_err(|_| internal("instances lock unavailable"))?;
        instances
            .get(&instance)
            .cloned()
            .ok_or_else(|| (-32004, "stale instance".into()))?
    };
    if params.get("action").and_then(Value::as_str) != Some("refresh") {
        return Err((-32601, "stale or unknown action".into()));
    }
    let mut result = probe(ctx, &current.url);
    if ctx.cancelled() {
        return Err((-32002, "cancelled".into()));
    }
    result.generation = ctx.shared.next_generation.fetch_add(1, Ordering::Relaxed);
    let mut instances = ctx
        .shared
        .instances
        .lock()
        .map_err(|_| internal("instances lock unavailable"))?;
    let session = instances
        .get_mut(&instance)
        .ok_or_else(|| (-32004, "stale instance".into()))?;
    if session.generation != current.generation {
        return Err((-32004, "stale instance".into()));
    }
    *session = result.clone();
    Ok(json!({"patch":state_json(&result)}))
}

/// 状态快照携带当前结果；关闭时先取快照再释放实例，迟到网络回执不能重新写入。
fn snapshot(ctx: &Ctx, params: &Value, close: bool) -> RpcResult {
    let instance = string_param(params, "instance")?;
    let mut instances = ctx
        .shared
        .instances
        .lock()
        .map_err(|_| internal("instances lock unavailable"))?;
    let state = instances
        .get(&instance)
        .cloned()
        .ok_or_else(|| (-32004, "stale instance".into()))?;
    if close {
        instances.remove(&instance);
    }
    Ok(json!({"state_version":1,"state":state_json(&state)}))
}

/// 只访问明确声明的域名；同时拒绝凭据、端口、片段和混淆主机名。
fn endpoint(ctx: &Ctx, config: &Value) -> Result<String, String> {
    let url = config
        .as_object()
        .and_then(|config| config.get("url"))
        .and_then(Value::as_str)
        .filter(|url| allowed(url))
        .ok_or_else(|| text(ctx, "invalid"))?;
    Ok(url.into())
}

/// 与旧示例的正则等价：仅 https://example.com 本身或其斜杠路径，不接受片段和端口。
fn allowed(raw: &str) -> bool {
    raw.strip_prefix("https://example.com")
        .is_some_and(|rest| rest.is_empty() || (rest.starts_with('/') && !rest.contains('#')))
}

/// 每次检查都有有限等待时间，不会把网络失败或重定向伪装成成功。
fn probe(ctx: &Ctx, url: &str) -> ResultState {
    let failed = || ResultState {
        url: url.into(),
        message: text(ctx, "failed"),
        ok: false,
        generation: 0,
    };
    let client = match Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(_) => return failed(),
    };
    match client.get(url).send() {
        Ok(response) if !response.status().is_redirection() => {
            let status = response.status();
            ResultState {
                url: url.into(),
                ok: status.is_success(),
                message: format!("{}: {}", text(ctx, "ok"), status.as_u16()),
                generation: 0,
            }
        }
        _ => failed(),
    }
}

fn state_json(state: &ResultState) -> Value {
    json!({"url":state.url,"message":state.message,"ok":state.ok})
}

/// 文案在运行时依据握手语言选择，非中文语言统一回退英文。
fn text(ctx: &Ctx, key: &str) -> String {
    match (ctx.chinese(), key) {
        (true, "ok") => "HTTP 状态码".into(),
        (true, "failed") => "无法访问此服务".into(),
        (true, "invalid") => "仅支持 https://example.com 地址".into(),
        (_, "ok") => "HTTP status".into(),
        (_, "failed") => "Could not reach this endpoint".into(),
        (_, "invalid") => "Only https://example.com URLs are allowed".into(),
        _ => String::new(),
    }
}

fn string_param(params: &Value, name: &str) -> Result<String, RpcError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| (-32602, format!("{name} required")))
}

fn valid_identity(params: &Value) -> bool {
    params.get("protocol").and_then(Value::as_u64) == Some(PROTOCOL_VERSION)
        && params
            .get("plugin")
            .and_then(|plugin| plugin.get("id"))
            .and_then(Value::as_str)
            == Some(PLUGIN_ID)
        && params
            .get("plugin")
            .and_then(|plugin| plugin.get("version"))
            .and_then(Value::as_str)
            == Some(PLUGIN_VERSION)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 主机类型只接受原示例允许的目标，绕过表单直接调用也不能扩大网络范围。
    #[test]
    fn endpoint_allowlist_matches_previous_example() {
        for url in [
            "https://example.com",
            "https://example.com/",
            "https://example.com/health",
            "https://example.com/a?b=1",
        ] {
            assert!(allowed(url), "{url}");
        }
        for url in [
            "https://example.com.evil/",
            "https://user@example.com/",
            "https://example.com:443/",
            "http://example.com/",
            "https://EXAMPLE.com/",
            "https://example.com/#fragment",
            "https://example.com\n.evil",
        ] {
            assert!(!allowed(url), "{url}");
        }
    }

    /// 缺损状态回退默认地址，避免恢复未验证目标。
    #[test]
    fn default_url_stays_on_the_allowlist() {
        assert!(allowed(DEFAULT_URL));
        assert!(!allowed("https://example.com.evil/"));
    }
}