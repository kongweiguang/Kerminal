//! 桌面进程唯一的异步执行器，PTY、MCP 与传输任务共用它。
//!
//! @author kongweiguang

use std::time::Duration;

use tokio::runtime::{Builder, Handle, Runtime};

/// 应用级 runtime 的所有权句柄；UI 线程从不通过它执行阻塞 IO。
pub struct ApplicationRuntime {
    runtime: Runtime,
}

impl ApplicationRuntime {
    /// 固定少量 worker 并启用 Tokio 所需 driver，避免每个服务各自创建 runtime。
    pub fn new() -> Result<Self, std::io::Error> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("kerminal-runtime")
            .enable_all()
            .build()?;
        Ok(Self { runtime })
    }

    /// 克隆轻量 runtime handle，服务仅持有执行能力而不拥有线程池生命周期。
    pub fn handle(&self) -> Handle {
        self.runtime.handle().clone()
    }

    /// 在 GPUI 事件循环退出后运行有序清理 futures，阻塞边界只存在于进程退出路径。
    pub fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    /// 有界等待后台任务结束，避免退出时永久挂住进程且不在 UI 线程调用。
    pub fn shutdown(self) {
        self.runtime.shutdown_timeout(Duration::from_secs(5));
    }
}
