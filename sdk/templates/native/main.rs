//! 官方 Rust SDK 统一负责分帧、握手、心跳、并发请求、取消和退出。
//! @author kongweiguang

use std::io;

use kerminal_plugin_sdk::{Error, Router};
use serde_json::json;

/// 模板保持无外部资源的静态视图；业务路由不重复实现 SDK 的协议及生命周期。
///
/// 插件启用后即被完全信任：Sidecar 以当前用户权限运行，具备完整文件、网络和
/// 进程能力；清单不再承担能力门禁，permissions 只保留描述用途。
#[tokio::main]
async fn main() -> io::Result<()> {
    let id = option_env!("KERMINAL_PLUGIN_ID").unwrap_or("__PLUGIN_ID__");
    let version = option_env!("KERMINAL_PLUGIN_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
    let mut router = Router::new(id, version);
    router
        .method("view.open", |_, _, _| async { Ok(json!({"data": {}})) })
        .map_err(route_error)?;
    for method in ["view.snapshot", "view.close"] {
        router
            .method(method, |_, _, _| async {
                Ok(json!({"state_version": 1, "state": {}}))
            })
            .map_err(route_error)?;
    }
    router.run().await
}

/// 注册失败属于启动失败，保留 SDK 文案到 stderr，绝不将诊断写入 stdout 协议流。
fn route_error(error: Error) -> io::Error {
    io::Error::other(error.message)
}