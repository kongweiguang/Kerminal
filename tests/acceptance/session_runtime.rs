//! 通过真实 SessionService 与本地 PTY 采集终端运行时验收数据。
//!
//! 该 example 仅使用独立临时目录和合成 shell 输出；不启动 GPUI、不读取用户配置，
//! PTY 响应与原始输出吞吐也不冒充 UI 帧延迟。
//!
//! @author kongweiguang

#[path = "session_runtime/mod.rs"]
mod acceptance;

/// 启动 async runtime 并将验收流程交给独立编排模块。
#[tokio::main]
async fn main() {
    acceptance::run().await;
}
