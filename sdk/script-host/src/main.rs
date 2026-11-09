// @author kongweiguang

mod async_host;
mod network;
mod storage;
mod timers;
mod web_native;

use std::cell::{Cell, RefCell};
use std::env;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use async_host::AsyncHost;
use kerminal_plugin_protocol::{
    InitializeParams, MAX_MANIFEST_BYTES, MAX_RPC_BYTES, MAX_VIEW_PATCH_BYTES, ManifestPolicy,
    PROTOCOL_VERSION, Permission, PluginRuntime, RpcNotification, RpcPeer, RpcRequest, RpcResponse,
    parse_manifest,
};
use rquickjs::{CatchResultExt, Context, Ctx, Exception, Function, Module, Runtime};
use serde_json::{Value, json};
use timers::TimerQueue;

const MAX_FRAME_BYTES: usize = MAX_RPC_BYTES;
const MAX_SCRIPT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_HEAP_BYTES: usize = 128 * 1024 * 1024;
const EXECUTION_LIMIT: Duration = Duration::from_secs(2);

struct Plugin {
    id: String,
    version: String,
    source: Vec<u8>,
    permissions: Vec<Permission>,
}

enum Input {
    Frame(Vec<u8>),
    Error(String),
    Closed,
    Native {
        id: u32,
        result: Result<Value, String>,
    },
}

#[derive(PartialEq, Eq)]
enum State {
    Starting,
    Initializing,
    Ready,
}

/// 每个进程只加载一个明确的插件目录，错误送到 stderr，避免污染 RPC 通道。
fn main() {
    if let Err(error) = run() {
        eprintln!("kerminal-plugin-host: {error}");
        std::process::exit(1);
    }
}

/// 把单线程 QuickJS 和有界输入队列分开，避免等待 stdin 时阻塞解释器任务。
fn run() -> Result<(), String> {
    let mut root = None;
    let mut official = false;
    for arg in env::args_os().skip(1) {
        if arg == "--official" && !official {
            official = true;
        } else if root.is_none() && arg != "--official" {
            root = Some(arg);
        } else {
            return Err("expected a plugin version directory and optional --official".into());
        }
    }
    let root = root.ok_or("expected plugin version directory")?;
    // 该标志只能由已经验证安装来源的组合根传入，绝不能从插件清单推断。
    let policy = if official {
        ManifestPolicy::Official
    } else {
        ManifestPolicy::ThirdParty
    };
    let plugin = load_plugin(Path::new(&root), policy)?;
    let (sender, input) = mpsc::sync_channel(64);
    let native = Rc::new(RefCell::new(AsyncHost::new(
        sender.clone(),
        plugin.permissions.clone(),
    )));
    read_stdin(sender);
    let runtime = Runtime::new().map_err(|error| error.to_string())?;
    runtime.set_memory_limit(MAX_HEAP_BYTES);
    let deadline = Rc::new(Cell::new(None::<Instant>));
    let ready = Rc::new(Cell::new(false));
    let timers = Rc::new(RefCell::new(TimerQueue::default()));
    let interrupt_deadline = deadline.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        interrupt_deadline
            .get()
            .is_some_and(|time| Instant::now() >= time)
    })));
    let context = Context::full(&runtime).map_err(|error| error.to_string())?;

    deadline.set(Some(Instant::now() + EXECUTION_LIMIT));
    let initialization = context.with(|ctx| {
        let result: rquickjs::Result<()> = (|| {
            let emit_ready = ready.clone();
            let emit = Function::new(ctx.clone(), move |ctx: Ctx<'_>, text: String| {
                if !emit_ready.get() {
                    return Err(Exception::throw_message(&ctx, "plugin is not initialized"));
                }
                emit_frame(&text).map_err(|error| Exception::throw_message(&ctx, &error))
            })?;
            ctx.globals().set("__kerminalEmit", emit)?;
            let scheduled = timers.clone();
            let schedule = Function::new(
                ctx.clone(),
                move |ctx: Ctx<'_>, delay: u32, repeat: bool| {
                    scheduled
                        .borrow_mut()
                        .schedule(delay, repeat)
                        .map_err(|error| Exception::throw_message(&ctx, &error))
                },
            )?;
            ctx.globals().set("__kerminalSchedule", schedule)?;
            let cancelled = timers.clone();
            let cancel = Function::new(ctx.clone(), move |id: u32| {
                cancelled.borrow_mut().cancel(id);
            })?;
            ctx.globals().set("__kerminalCancelTimer", cancel)?;
            let log = Function::new(ctx.clone(), |level: String, message: String| {
                eprintln!("[{level}] {message}");
            })?;
            ctx.globals().set("__kerminalLog", log)?;
            let start_host = native.clone();
            let start = Function::new(ctx.clone(), move |ctx: Ctx<'_>, text: String| {
                start_host
                    .borrow_mut()
                    .start(&text)
                    .map_err(|error| Exception::throw_message(&ctx, &error))
            })?;
            ctx.globals().set("__kerminalNativeStart", start)?;
            let cancel_host = native.clone();
            let cancel = Function::new(ctx.clone(), move |id: u32| {
                cancel_host.borrow().cancel(id);
            })?;
            ctx.globals().set("__kerminalNativeCancel", cancel)?;
            let web = Function::new(ctx.clone(), move |ctx: Ctx<'_>, text: String| {
                web_native::call(&text).map_err(|error| Exception::throw_message(&ctx, &error))
            })?;
            ctx.globals().set("__kerminalWebSync", web)?;
            let encode = Function::new(ctx.clone(), web_native::encode_binary)?;
            ctx.globals().set("__kerminalWebEncode", encode)?;
            let decode = Function::new(ctx.clone(), web_native::decode_binary)?;
            ctx.globals().set("__kerminalWebDecode", decode)?;
            ctx.eval::<(), _>(include_str!("web.js"))?;
            ctx.eval::<(), _>(include_str!("web-platform.js"))?;
            ctx.eval::<(), _>(include_str!("facilities.js"))?;
            ctx.eval::<(), _>(include_str!("bridge.js"))?;
            Ok(())
        })();
        result.catch(&ctx).map_err(|error| error.to_string())
    });
    deadline.set(None);
    initialization.map_err(|error| format!("plugin initialization failed: {error}"))?;

    let mut state = State::Starting;
    loop {
        let timer = if ready.get() {
            timers.borrow_mut().take_due()
        } else {
            None
        };
        if let Some(timer) = timer {
            invoke_timer(&context, &deadline, timer)?;
        }
        deadline.set(Some(Instant::now() + EXECUTION_LIMIT));
        let pending = runtime.execute_pending_job();
        let timed_out = deadline.get().is_some_and(|time| Instant::now() >= time);
        deadline.set(None);
        let pending = match pending {
            Ok(pending) => pending,
            Err(error) if timed_out => {
                // 先清除未捕获异常；QuickJS 不提供关联到具体 RPC 请求的微任务身份。
                error.0.with(|ctx| {
                    let _ = ctx.catch();
                });
                eprintln!("plugin JavaScript job execution exceeded limit");
                interrupt_active(&context, &deadline)?;
                true
            }
            Err(error) => return Err(format!("JavaScript job failed: {error}")),
        };
        let received = if pending {
            match input.try_recv() {
                Ok(message) => Some(message),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Input::Closed),
            }
        } else if ready.get() {
            match timers.borrow().wait_duration() {
                Some(wait) => match input.recv_timeout(wait) {
                    Ok(message) => Some(message),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => Some(Input::Closed),
                },
                None => Some(input.recv().unwrap_or(Input::Closed)),
            }
        } else {
            Some(input.recv().unwrap_or(Input::Closed))
        };
        let Some(message) = received else { continue };
        match message {
            Input::Frame(frame) => {
                if handle_frame(
                    &context, &deadline, &ready, &native, &plugin, &mut state, &frame,
                )? {
                    return Ok(());
                }
            }
            Input::Error(error) => return Err(error),
            Input::Closed => return Ok(()),
            Input::Native { id, result } => {
                if native.borrow_mut().finish(id) {
                    complete_native(&context, &deadline, id, result)?;
                }
            }
        }
    }
}

/// 等握手参数校验通过再执行入口，让脚本启动时即可读取可信语言和主题。
fn initialize_plugin(
    context: &Context,
    deadline: &Cell<Option<Instant>>,
    plugin: &Plugin,
    params: &InitializeParams,
    native: &RefCell<AsyncHost>,
) -> Result<(), String> {
    native.borrow_mut().set_root(&params.data_dir)?;
    deadline.set(Some(Instant::now() + EXECUTION_LIMIT));
    let result = context.with(|ctx| {
        let execution: rquickjs::Result<()> = (|| {
            let initialize: Function = ctx.globals().get("__kerminalInit")?;
            initialize.call::<_, ()>((
                json!({"locale":params.locale,"theme":params.theme}).to_string(),
            ))?;
            Module::evaluate(ctx.clone(), "plugin.mjs", plugin.source.clone())?.finish::<()>()?;
            Ok(())
        })();
        execution.catch(&ctx).map_err(|error| error.to_string())
    });
    deadline.set(None);
    result
}

/// 每个计时回调单独计时；超时停止该计时器并结束无法归属的活动请求。
fn invoke_timer(
    context: &Context,
    deadline: &Cell<Option<Instant>>,
    id: u32,
) -> Result<(), String> {
    deadline.set(Some(Instant::now() + EXECUTION_LIMIT));
    let result = context.with(|ctx| {
        let callback: Function = ctx
            .globals()
            .get("__kerminalTimer")
            .catch(&ctx)
            .map_err(|error| error.to_string())?;
        callback
            .call::<_, ()>((id,))
            .catch(&ctx)
            .map_err(|error| error.to_string())
    });
    let timed_out = deadline.get().is_some_and(|time| Instant::now() >= time);
    deadline.set(None);
    if let Err(error) = result {
        if timed_out {
            eprintln!("plugin timer execution exceeded limit: {error}");
            deadline.set(Some(Instant::now() + EXECUTION_LIMIT));
            let cancellation = context.with(|ctx| {
                let clear: Function = ctx
                    .globals()
                    .get("clearTimeout")
                    .catch(&ctx)
                    .map_err(|error| error.to_string())?;
                clear
                    .call::<_, ()>((id,))
                    .catch(&ctx)
                    .map_err(|error| error.to_string())
            });
            deadline.set(None);
            cancellation.map_err(|error| format!("failed to cancel interrupted timer: {error}"))?;
            interrupt_active(context, deadline)?;
        } else {
            eprintln!("plugin timer failed: {error}");
        }
    }
    Ok(())
}

/// 微任务或计时器中断没有可靠的请求归属，因此结束活动请求并丢弃迟到结果。
fn interrupt_active(context: &Context, deadline: &Cell<Option<Instant>>) -> Result<(), String> {
    deadline.set(Some(Instant::now() + EXECUTION_LIMIT));
    let result = context.with(|ctx| {
        let interrupt: Function = ctx
            .globals()
            .get("__kerminalInterruptActive")
            .catch(&ctx)
            .map_err(|error| error.to_string())?;
        interrupt
            .call::<_, ()>(())
            .catch(&ctx)
            .map_err(|error| error.to_string())
    });
    deadline.set(None);
    result.map_err(|error| format!("failed to interrupt active requests: {error}"))
}

/// 后台线程只传回 JSON 结果，Promise 的结算始终发生在 QuickJS 所属线程。
fn complete_native(
    context: &Context,
    deadline: &Cell<Option<Instant>>,
    id: u32,
    result: Result<Value, String>,
) -> Result<(), String> {
    let payload = match result {
        Ok(value) => json!({"ok":true,"value":value}),
        Err(error) => json!({"ok":false,"error":error}),
    }
    .to_string();
    deadline.set(Some(Instant::now() + EXECUTION_LIMIT));
    let completion = context.with(|ctx| {
        let callback: Function = ctx
            .globals()
            .get("__kerminalNativeComplete")
            .catch(&ctx)
            .map_err(|error| error.to_string())?;
        callback
            .call::<_, ()>((id, payload))
            .catch(&ctx)
            .map_err(|error| error.to_string())
    });
    deadline.set(None);
    completion.map_err(|error| format!("background completion failed: {error}"))
}

/// 统一使用协议清单规则，并限制入口指向插件目录内的普通文件。
fn load_plugin(root: &Path, policy: ManifestPolicy) -> Result<Plugin, String> {
    let root = fs::canonicalize(root).map_err(|error| format!("plugin directory: {error}"))?;
    let mut manifest_bytes = Vec::new();
    fs::File::open(root.join("kerminal-plugin.json"))
        .map_err(|error| format!("plugin manifest: {error}"))?
        .take((MAX_MANIFEST_BYTES + 1) as u64)
        .read_to_end(&mut manifest_bytes)
        .map_err(|error| format!("plugin manifest: {error}"))?;
    let manifest = parse_manifest(&manifest_bytes, policy)
        .map_err(|error| format!("plugin manifest: {error}"))?;
    let PluginRuntime::Script { main, .. } = &manifest.runtime else {
        return Err("manifest runtime is not script".into());
    };
    let relative = PathBuf::from(main);
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("script entry must be a relative path".into());
    }
    let script =
        fs::canonicalize(root.join(relative)).map_err(|error| format!("script entry: {error}"))?;
    if !script.starts_with(&root) {
        return Err("script entry escapes plugin directory".into());
    }
    let file = fs::File::open(script).map_err(|error| format!("script entry: {error}"))?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("script entry is not a regular file".into());
    }
    let mut source = Vec::new();
    file.take(MAX_SCRIPT_BYTES + 1)
        .read_to_end(&mut source)
        .map_err(|error| format!("script entry: {error}"))?;
    if source.len() as u64 > MAX_SCRIPT_BYTES {
        return Err("script entry exceeds size limit".into());
    }
    Ok(Plugin {
        id: manifest.id,
        version: manifest.version,
        source,
        permissions: manifest.permissions,
    })
}

/// 独立读取线程只缓存有界帧，过长或未完成的帧立即终止而非无限分配。
fn read_stdin(sender: SyncSender<Input>) {
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        loop {
            let mut frame = Vec::new();
            let read = (&mut input)
                .take((MAX_FRAME_BYTES + 2) as u64)
                .read_until(b'\n', &mut frame);
            match read {
                Ok(0) => {
                    send_input(&sender, Input::Closed);
                    return;
                }
                Ok(_) if frame.last() == Some(&b'\n') && frame.len() <= MAX_FRAME_BYTES + 1 => {
                    frame.pop();
                    if !send_input(&sender, Input::Frame(frame)) {
                        return;
                    }
                }
                Ok(_) => {
                    send_input(&sender, Input::Error("invalid or oversized frame".into()));
                    return;
                }
                Err(error) => {
                    send_input(&sender, Input::Error(format!("stdin: {error}")));
                    return;
                }
            }
        }
    });
}

/// 接收端退出后停止读取，避免后台线程继续持有输入数据。
fn send_input(sender: &SyncSender<Input>, input: Input) -> bool {
    sender.send(input).is_ok()
}

/// 握手先绑定清单身份，再把业务消息交给脚本，避免未初始化请求抢跑。
fn handle_frame(
    context: &Context,
    deadline: &Cell<Option<Instant>>,
    ready: &Cell<bool>,
    native: &RefCell<AsyncHost>,
    plugin: &Plugin,
    state: &mut State,
    frame: &[u8],
) -> Result<bool, String> {
    let message: Value = match serde_json::from_slice(frame) {
        Ok(message) => message,
        Err(_) => {
            emit_value(
                &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}),
            )?;
            return Ok(false);
        }
    };
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || !message.is_object() {
        emit_value(
            &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"invalid request"}}),
        )?;
        return Ok(false);
    }
    let id = message.get("id").and_then(Value::as_str);
    let method = message.get("method").and_then(Value::as_str);
    if let Some(method) = method {
        let valid = if message.get("id").is_some() {
            serde_json::from_value::<RpcRequest>(message.clone())
                .is_ok_and(|request| request.validate(RpcPeer::Host).is_ok())
        } else {
            serde_json::from_value::<RpcNotification>(message.clone())
                .is_ok_and(|notification| notification.validate().is_ok())
        };
        if !valid {
            emit_value(
                &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"invalid request id"}}),
            )?;
            return Ok(false);
        }
        let params = message.get("params");
        if params.is_some_and(|value| !value.is_object() && !value.is_array()) {
            if let Some(id) = id {
                send_error(id, -32602, "invalid params")?;
            }
            return Ok(false);
        }
        match method {
            "initialize" if *state == State::Starting && id.is_some() => {
                let initialization = params
                    .cloned()
                    .and_then(|value| serde_json::from_value::<InitializeParams>(value).ok())
                    .filter(|params| {
                        params.protocol == PROTOCOL_VERSION
                            && params.plugin.id == plugin.id
                            && params.plugin.version == plugin.version
                    });
                if let Some(params) = initialization {
                    if let Err(error) =
                        initialize_plugin(context, deadline, plugin, &params, native)
                    {
                        send_error(
                            id.unwrap_or_default(),
                            -32603,
                            "plugin initialization failed",
                        )?;
                        return Err(format!("plugin initialization failed: {error}"));
                    }
                    emit_value(
                        &json!({"jsonrpc":"2.0","id":id,"result":{"protocol":PROTOCOL_VERSION,"plugin":{"id":plugin.id,"version":plugin.version}}}),
                    )?;
                    *state = State::Initializing;
                } else {
                    send_error(
                        id.unwrap_or_default(),
                        -32602,
                        "plugin identity or protocol mismatch",
                    )?;
                }
                return Ok(false);
            }
            "initialized" if *state == State::Initializing && id.is_none() => {
                *state = State::Ready;
                ready.set(true);
            }
            "$/ping" if *state == State::Ready && id.is_some() => {
                emit_value(&json!({"jsonrpc":"2.0","id":id,"result":{}}))?;
                return Ok(false);
            }
            "shutdown" if *state == State::Ready && id.is_some() => {
                emit_value(&json!({"jsonrpc":"2.0","id":id,"result":{}}))?;
                return Ok(true);
            }
            _ if *state != State::Ready => {
                if let Some(id) = id {
                    send_error(id, -32600, "plugin not initialized")?;
                }
                return Ok(false);
            }
            _ => {}
        }
    } else if !id.is_some_and(|id| id.starts_with("p:") && id.len() > 2)
        || !serde_json::from_value::<RpcResponse>(message.clone())
            .is_ok_and(|response| response.validate().is_ok())
    {
        emit_value(
            &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"invalid response"}}),
        )?;
        return Ok(false);
    }

    deadline.set(Some(Instant::now() + EXECUTION_LIMIT));
    let result = context.with(|ctx| {
        let dispatch: Function = ctx
            .globals()
            .get("__kerminalDispatch")
            .catch(&ctx)
            .map_err(|error| error.to_string())?;
        dispatch
            .call::<_, ()>((message.to_string(),))
            .catch(&ctx)
            .map_err(|error| error.to_string())
    });
    deadline.set(None);
    if let Err(error) = result {
        if let Some(id) = id {
            deadline.set(Some(Instant::now() + EXECUTION_LIMIT));
            let cleanup = context.with(|ctx| {
                let abandon: Function = ctx
                    .globals()
                    .get("__kerminalAbandon")
                    .catch(&ctx)
                    .map_err(|error| error.to_string())?;
                abandon
                    .call::<_, ()>((id,))
                    .catch(&ctx)
                    .map_err(|error| error.to_string())
            });
            deadline.set(None);
            cleanup.map_err(|error| format!("failed to abandon request: {error}"))?;
            send_error(id, -32603, &error)?;
        } else {
            return Err(format!("plugin notification failed: {error}"));
        }
    }
    Ok(false)
}

/// 错误必须明确返回，不能把脚本失败包装成空的成功对象。
fn send_error(id: &str, code: i32, message: &str) -> Result<(), String> {
    emit_value(&json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
}

/// 统一限制宿主生成帧的大小，与脚本输出使用相同的传输规则。
fn emit_value(value: &Value) -> Result<(), String> {
    emit_frame(&value.to_string())
}

/// stdout 只承载完整 JSON 行，写入或刷新失败时让调用方明确失败。
fn emit_frame(text: &str) -> Result<(), String> {
    if text.len() > MAX_FRAME_BYTES || text.contains('\n') {
        return Err("invalid or oversized outgoing frame".into());
    }
    let message: Value =
        serde_json::from_str(text).map_err(|_| "invalid outgoing frame".to_owned())?;
    if message.get("method").and_then(Value::as_str) == Some("view.update") {
        let patch = message
            .pointer("/params/patch")
            .ok_or("missing view patch")?;
        if serde_json::to_vec(patch)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_VIEW_PATCH_BYTES
        {
            return Err("view patch exceeds size limit".into());
        }
    }
    let stdout = io::stdout();
    let mut output = stdout.lock();
    output
        .write_all(text.as_bytes())
        .and_then(|_| output.write_all(b"\n"))
        .and_then(|_| output.flush())
        .map_err(|error| format!("stdout: {error}"))
}
