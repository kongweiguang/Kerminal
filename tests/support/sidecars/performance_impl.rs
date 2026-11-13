// 持续视图更新夹具的 Sidecar 业务实现。
// @author kongweiguang
//
// 洪泛必须通过真实双向 RPC 才能计入宿主确认与背压；夹具保留有界在途窗口，
// 每个确认只更新本地计数，不能为了更快出数而伪造已确认数量。

use std::{
    collections::HashMap,
    fs,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

pub(super) const PLUGIN_ID: &str = "acme.notes";
pub(super) const PLUGIN_VERSION: &str = match option_env!("KERMINAL_PLUGIN_VERSION") {
    Some(version) => version,
    None => "1.0.0",
};
const DEFAULT_START_DELAY_MS: u64 = 8000;
const SEND_WINDOW_MS: u64 = 12000;
const UPDATES_PER_TICK: usize = 24;
const MAX_IN_FLIGHT: usize = 63;
const TICK_MS: u64 = 10;
const SETTLE_WINDOW_MS: u64 = 3000;

static SESSIONS: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();
static STOP: AtomicBool = AtomicBool::new(false);

/// 分派只接受正式视图方法；未知方法返回协议错误而不是让负载静默消失。
pub(super) fn dispatch(ctx: &Ctx, method: &str, params: Value) -> RpcResult {
    match method {
        "view.open" => open(ctx, &params),
        "view.close" => close(&params),
        "view.snapshot" => Ok(json!({"state_version":1,"state":{}})),
        _ => Err((-32601, "method not found".into())),
    }
}

/// 夹具不响应宿主通知；保留通用入口。
pub(super) fn on_notification(_ctx: &Ctx, _method: &str, _params: Value) -> Result<(), RpcError> {
    Ok(())
}

/// 退出时中止所有发送计时，避免进程关闭前继续占用宿主请求槽。
pub(super) fn on_shutdown(_shared: &Shared) {
    STOP.store(true, Ordering::SeqCst);
    if let Ok(sessions) = sessions().lock() {
        for stop in sessions.values() {
            stop.store(true, Ordering::SeqCst);
        }
    }
}

/// 打开登记实例并按配置的预热延迟启动发送线程，初始数据与正式渲染器约定一致。
fn open(ctx: &Ctx, params: &Value) -> RpcResult {
    let instance = string_param(params, "instance")?;
    let stop = Arc::new(AtomicBool::new(false));
    sessions()
        .lock()
        .map_err(|_| internal("sessions lock unavailable"))?
        .insert(instance.clone(), Arc::clone(&stop));
    let detached = Ctx {
        shared: Arc::clone(&ctx.shared),
        request: String::new(),
    };
    thread::spawn(move || run_flood(detached, instance, stop));
    Ok(json!({"data":{"seq":0,"done":false,"attempted":0,"acknowledged":0,"failed":0,"elapsed_ms":0}}))
}

/// 关闭先撤销发送状态；不存在实例返回 stale，不能把已关闭负载当作成功。
fn close(params: &Value) -> RpcResult {
    let instance = string_param(params, "instance")?;
    let mut sessions = sessions()
        .lock()
        .map_err(|_| internal("sessions lock unavailable"))?;
    let stop = sessions
        .remove(&instance)
        .ok_or_else(|| (-32004, "stale instance".into()))?;
    stop.store(true, Ordering::SeqCst);
    Ok(json!({"state_version":1,"state":{}}))
}

/// 发送线程区分窗口耗时与最终确认耗时；迟到回执计入确认耗时而不是抬高发送速率。
fn run_flood(ctx: Ctx, instance: String, stop: Arc<AtomicBool>) {
    thread::sleep(Duration::from_millis(start_delay_ms()));
    if stop.load(Ordering::SeqCst) || STOP.load(Ordering::SeqCst) {
        return;
    }
    let started = Instant::now();
    let started_unix = unix_ms();
    let in_flight = Arc::new(AtomicUsize::new(0));
    let (settle, outcomes) = std::sync::mpsc::channel::<(bool, Instant)>();
    let mut workers = Vec::new();
    let mut attempted = 0usize;
    let mut acknowledged = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut last_settled = started;
    let mut send_elapsed = 0u64;
    while send_elapsed < SEND_WINDOW_MS {
        if stop.load(Ordering::SeqCst) || STOP.load(Ordering::SeqCst) {
            break;
        }
        send_elapsed = started.elapsed().as_millis() as u64;
        for _ in 0..UPDATES_PER_TICK {
            if stop.load(Ordering::SeqCst) || STOP.load(Ordering::SeqCst) {
                break;
            }
            // 与正式 Sidecar 宿主同一边界：达到 64 槽前保留一格，超限请求只计数不发送。
            if in_flight.load(Ordering::SeqCst) >= MAX_IN_FLIGHT {
                skipped += 1;
                continue;
            }
            attempted += 1;
            let seq = attempted;
            in_flight.fetch_add(1, Ordering::SeqCst);
            let ctx = ctx.clone();
            let instance = instance.clone();
            let in_flight = Arc::clone(&in_flight);
            let settle = settle.clone();
            workers.push(thread::spawn(move || {
                let result = ctx.call(
                    "view.update",
                    json!({"instance":instance,"patch":{"seq":seq}}),
                );
                in_flight.fetch_sub(1, Ordering::SeqCst);
                let _ = settle.send((result.is_ok(), Instant::now()));
            }));
        }
        drain_outcomes(
            &outcomes,
            &mut acknowledged,
            &mut failed,
            &mut last_settled,
        );
        thread::sleep(Duration::from_millis(TICK_MS));
    }
    let deadline = Instant::now() + Duration::from_millis(SETTLE_WINDOW_MS);
    if stop.load(Ordering::SeqCst) || STOP.load(Ordering::SeqCst) {
        return;
    }
    while in_flight.load(Ordering::SeqCst) != 0 && Instant::now() < deadline {
        drain_outcomes(
            &outcomes,
            &mut acknowledged,
            &mut failed,
            &mut last_settled,
        );
        thread::sleep(Duration::from_millis(20));
        if stop.load(Ordering::SeqCst) || STOP.load(Ordering::SeqCst) {
            return;
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
    drain_outcomes(
        &outcomes,
        &mut acknowledged,
        &mut failed,
        &mut last_settled,
    );
    let pending = in_flight.load(Ordering::SeqCst);
    let observed = started.elapsed().as_millis() as u64;
    let finished = if pending == 0 {
        send_elapsed.max(last_settled.saturating_duration_since(started).as_millis() as u64)
    } else {
        observed
    };
    let report = json!({
        "instance":instance,
        "patch":{
            "done":true,
            "completed":pending == 0,
            "attempted":attempted,
            "acknowledged":acknowledged,
            "failed":failed,
            "in_flight":pending,
            "skipped_due_to_in_flight_limit":skipped,
            "send_elapsed_ms":send_elapsed,
            "completion_elapsed_ms":finished,
            "started_unix_ms":started_unix,
            "send_finished_unix_ms":started_unix + send_elapsed,
            "finished_unix_ms":started_unix + finished,
            "observed_unix_ms":unix_ms(),
        }
    });
    if ctx.call("view.update", report).is_err() {
        eprintln!("Performance fixture could not deliver its completion status.");
    }
}

/// 回执只在确认落到本地计数后释放；失败同样计入，不能用成功数掩盖宿主拒绝。
fn drain_outcomes(
    outcomes: &std::sync::mpsc::Receiver<(bool, Instant)>,
    acknowledged: &mut usize,
    failed: &mut usize,
    last_settled: &mut Instant,
) {
    for (ok, at) in outcomes.try_iter() {
        if ok {
            *acknowledged += 1;
        } else {
            *failed += 1;
        }
        *last_settled = at;
    }
}

/// 预热延迟由验收脚本写入包内资源，Sidecar 从自身位置读取而不依赖工作目录。
fn start_delay_ms() -> u64 {
    let Some(root) = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().and_then(|bin| bin.parent()).map(|root| root.to_owned()))
    else {
        return DEFAULT_START_DELAY_MS;
    };
    let path = root.join("performance.json");
    let Ok(text) = fs::read_to_string(path) else {
        return DEFAULT_START_DELAY_MS;
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| value.get("startDelayMs").and_then(Value::as_u64))
        .unwrap_or(DEFAULT_START_DELAY_MS)
}

fn sessions() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}