// 工作区笔记夹具的业务实现；1.0 与 1.1 共用源码，能力差异由构建时清单版本决定。
// @author kongweiguang
//
// 两代夹具必须能真实重启、保存各自草稿并保留旧实例语义；复制示例会掩盖版本边界，
// 因此这里保留独立实现，只复用通用协议与码点截断。

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use reqwest::{blocking::Client, redirect::Policy};
use serde_json::{Value, json};

pub(super) const PLUGIN_ID: &str = "acme.notes";
pub(super) const PLUGIN_VERSION: &str = match option_env!("KERMINAL_PLUGIN_VERSION") {
    Some(version) => version,
    None => "1.0.0",
};

#[derive(Clone)]
struct Session {
    view: String,
    text: String,
}

static SESSIONS: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();

/// 分派只接受协议声明的方法；未知方法返回类型化错误而不是静默成功。
pub(super) fn dispatch(ctx: &Ctx, method: &str, params: Value) -> RpcResult {
    match method {
        "view.open" => open_view(ctx, &params),
        "view.action" => action(ctx, &params),
        "view.snapshot" => snapshot(ctx, &params, false),
        "view.close" => snapshot(ctx, &params, true),
        "command.run" => command(ctx),
        "settings.changed" => {
            settings_changed(ctx, &params)?;
            Ok(json!({}))
        }
        _ => Err((-32601, "method not found".into())),
    }
}

/// 宿主设置通知与请求走同一实现，失败只记录标准错误且不改变编辑器草稿。
pub(super) fn on_notification(ctx: &Ctx, method: &str, params: Value) -> Result<(), RpcError> {
    if method == "settings.changed" {
        settings_changed(ctx, &params)?;
    }
    Ok(())
}

/// 退出时进程即将结束，状态无需持久化；保留入口便于与通用协议明确边界。
pub(super) fn on_shutdown(_shared: &Shared) {}

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
    let text = match restored {
        Some(value) => note_text(Some(value)),
        None => saved_text.clone(),
    };
    sessions()
        .lock()
        .map_err(|_| internal("sessions lock unavailable"))?
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
        Ok(json!({"data":{"saved":saved_text,"showPreview":show_preview}}))
    } else {
        let mut data = json!({"text":text,"marker":PLUGIN_VERSION});
        if PLUGIN_VERSION == "1.1.0" {
            data["codePointCount"] = json!("");
        }
        Ok(json!({"data":data}))
    }
}

/// 动作只使用发起时的实例快照，不覆盖其他编辑器的未保存输入。
fn action(ctx: &Ctx, params: &Value) -> RpcResult {
    let instance = string_param(params, "instance")?;
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| (-32602, "action required".into()))?
        .to_owned();
    let draft = params
        .get("data")
        .and_then(|data| data.get("text"))
        .and_then(Value::as_str)
        .map(|value| note_text(Some(value)));
    let session = {
        let mut sessions = sessions()
            .lock()
            .map_err(|_| internal("sessions lock unavailable"))?;
        let session = sessions
            .get_mut(&instance)
            .ok_or_else(|| (-32004, "stale instance".into()))?;
        if let Some(text) = draft {
            session.text = text;
        }
        session.clone()
    };
    match action.as_str() {
        "save" => {
            let saved_text = session.text.clone();
            ctx.call("state.set", json!({"state":{"text":saved_text}}))?;
            for instance in panel_instances()? {
                ctx.call(
                    "view.update",
                    json!({"instance":instance,"patch":{"saved":saved_text}}),
                )?;
            }
            Ok(json!({"toast":text(ctx, "saved")}))
        }
        "check" => Ok(json!({"toast":check_service(ctx)})),
        // 只有 1.1.0 同时具备按钮与真实复制行为，旧版本不能凭空获得能力。
        "copy" if PLUGIN_VERSION == "1.1.0" => {
            ctx.call("clipboard.write", json!({"text":session.text}))?;
            Ok(json!({"toast":text(ctx, "copied")}))
        }
        // 统计删除任何共享资源访问，只把当前实例动作快照的 Unicode 标量数送回视图。
        "count-code-points" if PLUGIN_VERSION == "1.1.0" => {
            let count = session.text.chars().count();
            ctx.call(
                "view.update",
                json!({"instance":instance,"patch":{"codePointCount":count}}),
            )?;
            Ok(json!({}))
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
    for instance in panel_instances()? {
        ctx.call(
            "view.update",
            json!({"instance":instance,"patch":{"showPreview":show_preview}}),
        )?;
    }
    Ok(())
}

/// 快照优先使用宿主附带的当前数据；关闭时先快照再清理，避免状态串入其他标签。
fn snapshot(_ctx: &Ctx, params: &Value, close: bool) -> RpcResult {
    let instance = string_param(params, "instance")?;
    let current = params
        .get("data")
        .and_then(|data| data.get("text"))
        .and_then(Value::as_str)
        .map(|value| note_text(Some(value)));
    let mut sessions = sessions()
        .lock()
        .map_err(|_| internal("sessions lock unavailable"))?;
    let session = sessions
        .get_mut(&instance)
        .ok_or_else(|| (-32004, "stale instance".into()))?;
    if let Some(text) = current {
        session.text = text;
    }
    let state = json!({"state_version":1,"state":{"text":session.text.clone()}});
    if close {
        sessions.remove(&instance);
    }
    Ok(state)
}

/// 命令复用同一网络检查，结果由宿主通知而不是更改当前编辑草稿。
fn command(ctx: &Ctx) -> RpcResult {
    let message = check_service(ctx);
    ctx.call("ui.notify", json!({"level":"info","message":message}))?;
    Ok(json!({}))
}

/// 网络检查由 Sidecar 自行发起；插件启用后没有宿主白名单，失败仍返回可读文案。
fn check_service(ctx: &Ctx) -> String {
    let client = match Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(_) => return text(ctx, "unavailable"),
    };
    match client.head("https://example.com/").send() {
        Ok(response) if response.status().is_success() => text(ctx, "available"),
        _ => text(ctx, "unavailable"),
    }
}

/// 面板实例按当前会话快照筛选，调用宿主前释放锁避免回执线程等待。
fn panel_instances() -> Result<Vec<String>, RpcError> {
    let sessions = sessions()
        .lock()
        .map_err(|_| internal("sessions lock unavailable"))?;
    Ok(sessions
        .iter()
        .filter(|(_, session)| session.view == "notes.panel")
        .map(|(instance, _)| instance.clone())
        .collect())
}

fn sessions() -> &'static Mutex<HashMap<String, Session>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 文案在运行时依据握手语言选择，非中文语言统一回退英文。
fn text(ctx: &Ctx, key: &str) -> String {
    let english = match key {
        "available" => "example.com is reachable",
        "unavailable" => "Could not reach example.com",
        "saved" => "Note saved",
        "copied" => "Note copied",
        _ => return String::new(),
    };
    if ctx.chinese() {
        match key {
            "available" => "example.com 可访问",
            "unavailable" => "无法访问 example.com",
            "saved" => "笔记已保存",
            "copied" => "笔记已复制",
            _ => english,
        }
        .into()
    } else {
        english.into()
    }
}