//! MCP listener 状态与生命周期控制面板。
//!
//! @author kongweiguang

use std::{future::Future, sync::Arc, task::Poll, time::Duration};

use gpui_kit::{Context, IntoElement, Render, Window};
use kerminal_application::{
    AppError, ApplicationTaskExecutor,
    mcp_runtime::{Instant, McpRuntimeControl, McpRuntimeSnapshot, McpRuntimeState},
};
use tokio_util::sync::CancellationToken;

mod render;

/// MCP 服务面板只持有应用端口；listener、endpoint 同步与重试 owner 留在 runtime adapter。
pub struct McpRuntimePanel {
    runtime: Arc<dyn McpRuntimeControl>,
    executor: ApplicationTaskExecutor,
    snapshot: McpRuntimeSnapshot,
    operation: Option<RuntimeOperation>,
    start_cancellation: Option<CancellationToken>,
    status: Option<String>,
    generation: u64,
}

/// 单次 UI 操作的世代号，防止旧任务结果覆盖用户之后发起的新操作。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeOperation {
    Start,
    Stop,
}

impl McpRuntimePanel {
    /// 用桌面装配的唯一 runtime control 创建面板，不因打开页面而启动 listener。
    pub fn new(runtime: Arc<dyn McpRuntimeControl>, executor: ApplicationTaskExecutor) -> Self {
        let snapshot = runtime.snapshot();
        Self {
            runtime,
            executor,
            snapshot,
            operation: None,
            start_cancellation: None,
            status: None,
            generation: 0,
        }
    }

    /// Root 可在 Agent 操作后刷新同一 owner 的实际状态，不重新构造 server 或服务图。
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.snapshot = self.runtime.snapshot();
        if self.operation.is_none() {
            self.status = self
                .snapshot
                .failure_reason
                .clone()
                .or_else(|| self.snapshot.unavailable_reason.clone());
        }
        cx.notify();
    }

    /// 显式开始 MCP listener；请求取消后 controller 仍负责收束或保留原始 owner。
    pub fn start(&mut self, cx: &mut Context<Self>) {
        if self.operation.is_some()
            || !matches!(
                self.snapshot.state,
                McpRuntimeState::Stopped | McpRuntimeState::Synchronizing | McpRuntimeState::Failed
            )
        {
            return;
        }
        let user_cancellation = CancellationToken::new();
        self.start_cancellation = Some(user_cancellation.clone());
        self.begin_operation(RuntimeOperation::Start, cx);

        let runtime = Arc::clone(&self.runtime);
        let executor = self.executor.clone();
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |task_cancellation| async move {
                    let operation_cancellation = CancellationToken::new();
                    let operation = runtime.start(operation_cancellation.clone());
                    await_operation_with_cooperative_cancellation(
                        operation,
                        operation_cancellation,
                        user_cancellation,
                        task_cancellation,
                    )
                    .await
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.finish_operation(generation, result, cx)
            });
        })
        .detach();
    }

    /// 取消正在等待的 start；adapter 收到协作取消后仍等待 listener/context 收尾结果。
    pub fn cancel_start(&mut self, cx: &mut Context<Self>) {
        if let Some(cancellation) = self.start_cancellation.as_ref() {
            cancellation.cancel();
            self.status = Some(
                crate::locale::text(
                    "正在请求取消启动并等待资源收尾…",
                    "Requesting start cancellation and waiting for cleanup…",
                )
                .to_string(),
            );
            cx.notify();
        }
    }

    /// 停止 listener 使用有界 deadline；超时保留 controller owner，界面允许再次点击重试。
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if self.operation.is_some()
            || self.snapshot.state == McpRuntimeState::Unavailable
            || (self.snapshot.endpoint.is_none() && !self.snapshot.endpoint_sync_pending)
        {
            return;
        }
        self.begin_operation(RuntimeOperation::Stop, cx);

        let runtime = Arc::clone(&self.runtime);
        let executor = self.executor.clone();
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_task_cancellation| async move {
                    runtime
                        .stop_until(Instant::now() + Duration::from_secs(5))
                        .await
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.finish_operation(generation, result, cx)
            });
        })
        .detach();
    }

    /// 复制仅在服务完整运行时发布的客户端 JSON；stopped/pending 状态不会复制失效地址。
    pub fn copy_client_config(&mut self, cx: &mut Context<Self>) {
        let Some(client_json) = self.snapshot.client_json.clone() else {
            self.status = Some(
                crate::locale::text(
                    "服务运行并完成 Agent 同步后才能复制配置。",
                    "Start the server and finish Agent synchronization before copying the config.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(client_json));
        self.status =
            Some(crate::locale::text("客户端配置已复制。", "Client config copied.").to_string());
        cx.notify();
    }

    /// 为异步操作建立世代并清除上一轮状态，避免重复操作触及同一资源 owner。
    fn begin_operation(&mut self, operation: RuntimeOperation, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.operation = Some(operation);
        self.status = None;
        cx.notify();
    }

    /// 只接受当前操作结果并从 controller 重新读取状态，不把本地结果当成 listener 事实。
    fn finish_operation(
        &mut self,
        generation: u64,
        result: Result<McpRuntimeSnapshot, AppError>,
        cx: &mut Context<Self>,
    ) {
        if self.generation != generation {
            return;
        }
        let completed_operation = self.operation.take();
        self.start_cancellation = None;
        self.snapshot = self.runtime.snapshot();
        self.status = match result {
            Ok(snapshot) => {
                self.snapshot = snapshot;
                match completed_operation {
                    Some(RuntimeOperation::Start) => Some(
                        crate::locale::text("MCP 服务已启动。", "MCP server started.").to_string(),
                    ),
                    Some(RuntimeOperation::Stop) => Some(
                        crate::locale::text("MCP 服务已停止。", "MCP server stopped.").to_string(),
                    ),
                    None => None,
                }
            }
            Err(error) => Some(error.public_message().to_owned()),
        }
        .or_else(|| self.snapshot.failure_reason.clone())
        .or_else(|| self.snapshot.unavailable_reason.clone());
        cx.notify();
    }
}

impl Render for McpRuntimePanel {
    /// 浅色、深色与跟随系统共用语义主题令牌，状态与可复制 endpoint 始终来自真实 owner。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_panel(window, cx)
    }
}

/// 取消时只通知运行用例并继续等待其清理结果，避免丢弃 listener 收尾 future。
async fn await_operation_with_cooperative_cancellation<T, Fut>(
    operation: Fut,
    operation_cancellation: CancellationToken,
    user_cancellation: CancellationToken,
    task_cancellation: CancellationToken,
) -> T
where
    T: Send,
    Fut: Future<Output = T> + Send,
{
    let mut operation = Box::pin(operation);
    let mut user_cancelled = Box::pin(user_cancellation.cancelled());
    let mut task_cancelled = Box::pin(task_cancellation.cancelled());
    let mut cancellation_sent = false;

    std::future::poll_fn(|context| {
        if let Poll::Ready(result) = operation.as_mut().poll(context) {
            return Poll::Ready(result);
        }
        if !cancellation_sent
            && (user_cancelled.as_mut().poll(context).is_ready()
                || task_cancelled.as_mut().poll(context).is_ready())
        {
            cancellation_sent = true;
            operation_cancellation.cancel();
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
#[path = "../../../tests/unit/mcp_runtime_cancellation.rs"]
mod cancellation_tests;
