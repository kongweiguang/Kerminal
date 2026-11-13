//! 可选 Rust Sidecar：由官方 SDK 接管握手、分帧、并发与取消。
//! @author kongweiguang

use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
};

use kerminal_plugin_sdk::{Error, Router};
use serde_json::{Value, json};

/// 插件启用后即被完全信任：Sidecar 以当前用户权限运行，具备完整文件、网络和
/// 进程能力；清单不再承担能力门禁。状态按实例保存，多个标签同时打开时不会互相覆盖。
#[tokio::main]
async fn main() -> io::Result<()> {
    let id = option_env!("KERMINAL_PLUGIN_ID").unwrap_or("__PLUGIN_ID__");
    let version = option_env!("KERMINAL_PLUGIN_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
    let sessions: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut router = Router::new(id, version);

    let open_sessions = Arc::clone(&sessions);
    router
        .method("view.open", move |_, params, _| {
            let sessions = Arc::clone(&open_sessions);
            async move {
                let instance = instance(&params)?;
                let text = bounded(
                    params
                        .get("state")
                        .and_then(|state| state.get("text"))
                        .and_then(Value::as_str),
                );
                sessions
                    .lock()
                    .map_err(|_| Error::new(-32100, "session lock unavailable"))?
                    .insert(instance, text.clone());
                Ok(json!({"data": {"text": text}}))
            }
        })
        .map_err(route_error)?;

    let action_sessions = Arc::clone(&sessions);
    router
        .method("view.action", move |_, params, _| {
            let sessions = Arc::clone(&action_sessions);
            async move {
                let instance = instance(&params)?;
                if params.get("action").and_then(Value::as_str) != Some("save") {
                    return Err(Error::new(-32601, "unknown action"));
                }
                let text = bounded(
                    params
                        .get("data")
                        .and_then(|data| data.get("text"))
                        .and_then(Value::as_str),
                );
                let mut sessions = sessions
                    .lock()
                    .map_err(|_| Error::new(-32100, "session lock unavailable"))?;
                let session = sessions
                    .get_mut(&instance)
                    .ok_or_else(|| Error::new(-32004, "stale instance"))?;
                *session = text.clone();
                Ok(json!({"patch": {"text": text}}))
            }
        })
        .map_err(route_error)?;

    let snapshot_sessions = Arc::clone(&sessions);
    router
        .method("view.snapshot", move |_, params, _| {
            let sessions = Arc::clone(&snapshot_sessions);
            async move {
                let instance = instance(&params)?;
                let sessions = sessions
                    .lock()
                    .map_err(|_| Error::new(-32100, "session lock unavailable"))?;
                let text = sessions
                    .get(&instance)
                    .ok_or_else(|| Error::new(-32004, "stale instance"))?
                    .clone();
                Ok(json!({"state_version": 1, "state": {"text": text}}))
            }
        })
        .map_err(route_error)?;

    let close_sessions = Arc::clone(&sessions);
    router
        .method("view.close", move |_, params, _| {
            let sessions = Arc::clone(&close_sessions);
            async move {
                let instance = instance(&params)?;
                let mut sessions = sessions
                    .lock()
                    .map_err(|_| Error::new(-32100, "session lock unavailable"))?;
                let text = sessions
                    .remove(&instance)
                    .ok_or_else(|| Error::new(-32004, "stale instance"))?;
                Ok(json!({"state_version": 1, "state": {"text": text}}))
            }
        })
        .map_err(route_error)?;

    router.run().await
}

/// 宿主只允许字符串实例 ID；缺失或错误类型必须返回协议错误而不是创建匿名会话。
fn instance(params: &Value) -> Result<String, Error> {
    params
        .get("instance")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::new(-32602, "instance required"))
}

/// 截断到协议允许的内容上限，避免恢复状态或补丁无限扩张。
fn bounded(value: Option<&str>) -> String {
    value.unwrap_or_default().chars().take(10_000).collect()
}

/// 注册失败属于启动失败，保留 SDK 文案到 stderr，绝不将诊断写入 stdout 协议流。
fn route_error(error: Error) -> io::Error {
    io::Error::other(error.message)
}