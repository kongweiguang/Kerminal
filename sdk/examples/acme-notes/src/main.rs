//! 工作区笔记示例的 Rust Sidecar：与宿主共用 JSON-RPC v1 线协议。
//! @author kongweiguang
//!
//! 插件启用后即被完全信任：Sidecar 以当前用户权限运行，具备完整文件、网络和
//! 进程能力；清单不再承担能力门禁，网络与剪贴板操作都由 Sidecar 自身发起。

use std::{
    collections::{HashMap, HashSet},
    env,
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

use reqwest::{blocking::Client, redirect::Policy};
use serde_json::{Value, json};

const PLUGIN_ID: &str = "acme.notes";
const PLUGIN_VERSION: &str = match option_env!("KERMINAL_PLUGIN_VERSION") {
    Some(version) => version,
    None => "1.0.0",
};
const PROTOCOL_VERSION: u64 = 1;
const MAX_FRAME: usize = 4 * 1024 * 1024;
const MAX_PENDING: usize = 64;
const MAX_TEXT: usize = 32_000;

type RpcError = (i32, String);
type RpcResult = Result<Value, RpcError>;

#[derive(Clone)]
struct Session {
    view: String,
    text: String,
}

struct Shared {
    output: Mutex<io::Stdout>,
    pending: Mutex<HashMap<String, mpsc::Sender<Result<Value, RpcError>>>>,
    next: AtomicU64,
    locale: Mutex<String>,
    instances: Mutex<HashMap<String, Session>>,
    active: Mutex<HashSet<String>>,
    cancelled: Mutex<HashSet<String>>,
}

#[derive(Clone)]
struct Ctx {
    shared: Arc<Shared>,
    request: String,
}

impl Ctx {
    /// 宿主能力必须等回执才能继续业务；等待期间持续检查取消，避免关闭后的动作仍写入视图。
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

    /// 取消只作用于当前请求，不影响同一进程中的其他视图。
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
        eprintln!("acme-notes: {error}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let shared = Arc::new(Shared {
        output: Mutex::new(io::stdout()),
        pending: Mutex::new(HashMap::new()),
        next: AtomicU64::new(1),
        locale: Mutex::new("en".into()),
        instances: Mutex::new(HashMap::new()),
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
            // 设置变更只更新侧栏，不重开编辑器；失败只记录在标准错误。
            if initialized && method == "settings.changed" {
                let ctx = Ctx {
                    shared: Arc::clone(&shared),
                    request: String::new(),
                };
                let params = params.clone();
                notifications.push(thread::spawn(move || {
                    if let Err((_, message)) = settings_changed(&ctx, &params) {
                        eprintln!("acme-notes: settings update failed: {message}");
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
    join_all(&mut notifications);
    Ok(())
}

/// 业务分派只接受协议列出的方法；未知方法返回类型化错误而不是静默成功。
fn dispatch(ctx: &Ctx, method: &str, params: Value) -> RpcResult {
    match method {
        "view.open" => open_view(ctx, &params),
        "view.action" => action(ctx, &params),
        "view.snapshot" => snapshot(ctx, &params, false),
        "view.close" => snapshot(ctx, &params, true),
        "command.run" => command(ctx),
        _ => Err((-32601, "method not found".into())),
    }
}

/// 打开前读取插件状态；实例快照优先，避免重启后覆盖未保存草稿。
fn open_view(ctx: &Ctx, params: &Value) -> RpcResult {
    let instance = string_param(params, "instance")?;
    let view = params
        .get("view")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let saved = ctx.call("state.get", json!({}))?;
    let saved = saved.get("state").cloned().unwrap_or(saved);
    let saved_text = note_text(saved.get("text").and_then(Value::as_str));
    let restored = params
        .get("state")
        .and_then(|state| state.get("text"))
        .and_then(Value::as_str);
    let text = note_text(restored.or(saved_text.as_deref()));
    ctx.shared
        .instances
        .lock()
        .map_err(|_| internal("instances lock unavailable"))?
        .insert(
            instance,
            Session {
                view: view.clone(),
                text: text.clone(),
            },
        );
    if view == "notes.panel" {
        let reply = ctx.call("settings.get", json!({}))?;
        let settings = reply.get("settings").cloned().unwrap_or(reply);
        let show_preview = settings
            .get("showPreview")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        Ok(json!({"data":{"saved":saved_text.unwrap_or_default(),"showPreview":show_preview}}))
    } else {
        Ok(json!({"data":{"text":text}}))
    }
}

/// 保存只写当前实例的草稿，随后刷新所有侧栏，不改变其他编辑器的未提交输入。
fn action(ctx: &Ctx, params: &Value) -> RpcResult {
    let instance = string_param(params, "instance")?;
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| (-32602, "action required".into()))?
        .to_owned();
    let text = params
        .get("data")
        .and_then(|data| data.get("text"))
        .and_then(Value::as_str)
        .map(note_text);
    let session = {
        let mut instances = ctx
            .shared
            .instances
            .lock()
            .map_err(|_| internal("instances lock unavailable"))?;
        let session = instances
            .get_mut(&instance)
            .ok_or_else(|| (-32004, "stale instance".into()))?;
        if let Some(text) = text {
            session.text = text;
        }
        session.clone()
    };
    match action.as_str() {
        "save" => {
            let saved_text = session.text.clone();
            ctx.call("state.set", json!({"state":{"text":saved_text}}))?;
            let panels = {
                let instances = ctx
                    .shared
                    .instances
                    .lock()
                    .map_err(|_| internal("instances lock unavailable"))?;
                instances
                    .iter()
                    .filter(|(_, target)| target.view == "notes.panel")
                    .map(|(instance, _)| instance.clone())
                    .collect::<Vec<_>>()
            };
            for instance in panels {
                ctx.call(
                    "view.update",
                    json!({"instance":instance,"patch":{"saved":saved_text}}),
                )?;
            }
            Ok(json!({"toast":text(ctx, "saved")}))
        }
        "check" => Ok(json!({"toast":check_service(ctx)})),
        // 只有 1.1.0 版本同时具备按钮和真实复制行为，旧版本不能凭空获得能力。
        "copy" if PLUGIN_VERSION == "1.1.0" => {
            ctx.call("clipboard.write", json!({"text":session.text}))?;
            Ok(json!({"toast":text(ctx, "copied")}))
        }
        _ => Err((-32601, "unknown action".into())),
    }
}

/// 设置变动只影响侧栏预览，不改写编辑器草稿。
fn settings_changed(ctx: &Ctx, params: &Value) -> Result<(), RpcError> {
    let settings = params.get("settings").cloned().unwrap_or(Value::Null);
    let show_preview = settings
        .get("showPreview")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let panels = {
        let instances = ctx
            .shared
            .instances
            .lock()
            .map_err(|_| internal("instances lock unavailable"))?;
        instances
            .iter()
            .filter(|(_, target)| target.view == "notes.panel")
            .map(|(instance, _)| instance.clone())
            .collect::<Vec<_>>()
    };
    for instance in panels {
        ctx.call(
            "view.update",
            json!({"instance":instance,"patch":{"showPreview":show_preview}}),
        )?;
    }
    Ok(())
}

/// 快照优先使用宿主附带的当前数据；旧宿主只能恢复最近一次动作。
fn snapshot(ctx: &Ctx, params: &Value, close: bool) -> RpcResult {
    let instance = string_param(params, "instance")?;
    let current = params
        .get("data")
        .and_then(|data| data.get("text"))
        .and_then(Value::as_str)
        .map(note_text);
    let mut instances = ctx
        .shared
        .instances
        .lock()
        .map_err(|_| internal("instances lock unavailable"))?;
    let session = instances
        .get_mut(&instance)
        .ok_or_else(|| (-32004, "stale instance".into()))?;
    if let Some(text) = current {
        session.text = text;
    }
    let state = json!({"state_version":1,"state":{"text":session.text.clone()}});
    if close {
        instances.remove(&instance);
    }
    Ok(state)
}

/// 命令复用同一网络检查，结果由宿主通知而不是更改当前编辑草稿。
fn command(ctx: &Ctx) -> RpcResult {
    let message = check_service(ctx);
    ctx.call("ui.notify", json!({"level":"info","message":message}))?;
    Ok(json!({}))
}

/// 网络失败与重定向都按不可达处理，和原脚本的 `redirect: "error"` 语义一致。
fn check_service(ctx: &Ctx) -> String {
    let client = match Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(_) => return text(ctx, "unavailable"),
    };
    match client.get("https://example.com/").send() {
        Ok(response) if response.status().is_success() => text(ctx, "available"),
        _ => text(ctx, "unavailable"),
    }
}

/// 文案在运行时依据握手语言选择，非中文语言统一回退英文。
fn text(ctx: &Ctx, key: &str) -> String {
    let value = match (ctx.chinese(), key) {
        (true, "available") => "example.com 可访问",
        (true, "unavailable") => "无法访问 example.com",
        (true, "saved") => "笔记已保存",
        (true, "copied") => "笔记已复制",
        (_, "available") => "example.com is reachable",
        (_, "unavailable") => "Could not reach example.com",
        (_, "saved") => "Note saved",
        (_, "copied") => "Note copied",
        _ => "",
    };
    value.into()
}

/// 未知状态只作为空对象处理，避免恢复旧数据时触发类型异常。
fn note_text(value: Option<&str>) -> String {
    value.unwrap_or_default().chars().take(MAX_TEXT).collect()
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
    emit(
        shared,
        &json!({"jsonrpc":"2.0","id":id,"result":result}),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 快照内容按字符截断，不能把多字节中文从中间切断。
    #[test]
    fn note_text_truncates_by_characters() {
        let source = "中".repeat(MAX_TEXT + 10);
        assert_eq!(note_text(Some(source.as_str())), "中".repeat(MAX_TEXT));
        assert_eq!(note_text(None), "");
    }

    /// 非中文语言回退英文，未知键返回空串而不是泄漏内部状态。
    #[test]
    fn localized_messages_fall_back_to_english() {
        let shared = Arc::new(Shared {
            output: Mutex::new(io::stdout()),
            pending: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            locale: Mutex::new("fr".into()),
            instances: Mutex::new(HashMap::new()),
            active: Mutex::new(HashSet::new()),
            cancelled: Mutex::new(HashSet::new()),
        });
        let ctx = Ctx {
            shared,
            request: String::new(),
        };
        assert_eq!(text(&ctx, "available"), "example.com is reachable");
        assert_eq!(text(&ctx, "unknown"), "");
    }
}