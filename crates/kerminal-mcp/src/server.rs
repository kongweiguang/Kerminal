//! MCP Streamable HTTP 服务宿主及桌面生命周期接口。
//!
//! @author kongweiguang

use std::{
    io::ErrorKind,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header::ORIGIN},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    serve,
};
use kerminal_application::{AppError, AppResult, PublicErrorCode, agents::AgentService};
use kerminal_domain::identity::AgentSessionId;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig,
    session::{SessionManager, local::LocalSessionManager},
    tower::StreamableHttpService,
};
use tokio::{
    net::TcpListener,
    runtime::Handle,
    task::{JoinHandle, JoinSet},
    time::{Instant, timeout_at},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{McpServices, protocol::KerminalMcpHandler};

const DEFAULT_MCP_PORT: u16 = 37657;
const MCP_PORT_SCAN_LIMIT: u16 = 64;
const MCP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// MCP 监听配置。端口 0 表示调用方显式请求系统分配可用端口。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct McpServerConfig {
    bind_address: SocketAddr,
}

impl McpServerConfig {
    /// 仅接受旧版规范化后的 IPv4 loopback 地址，保持本地 MCP 的 Host 边界。
    pub fn new(bind_address: SocketAddr) -> AppResult<Self> {
        if bind_address.ip() != Ipv4Addr::LOCALHOST {
            return Err(AppError::invalid("MCP bind address must be loopback"));
        }
        Ok(Self { bind_address })
    }

    /// 返回显式配置的绑定地址，供宿主记录诊断但不写回用户配置。
    pub const fn bind_address(self) -> SocketAddr {
        self.bind_address
    }
}

impl Default for McpServerConfig {
    /// 默认复用旧版 127.0.0.1:37657 入口，端口冲突时沿用向后扫描策略。
    fn default() -> Self {
        Self {
            bind_address: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), DEFAULT_MCP_PORT),
        }
    }
}

/// MCP 服务构造入口；持有服务生命周期而不占用 GPUI 线程。
pub struct McpServer;

impl McpServer {
    /// 复用应用 runtime 和固定工具目录；每个 RMCP worker 登记生命周期令牌以便统一回收。
    pub async fn start(
        runtime: &Handle,
        services: McpServices,
        config: McpServerConfig,
    ) -> AppResult<McpServerHandle> {
        let listener = bind_listener(config.bind_address).await?;
        let local_address = listener.local_addr().map_err(|error| {
            tracing::error!(%error, "failed to read MCP listener address");
            AppError::infrastructure("MCP listener address unavailable")
        })?;
        let allowed_origin = format!("http://{local_address}");

        let cancellation = CancellationToken::new();
        let service_lifetimes = TaskTracker::new();
        let sessions = Arc::new(LocalSessionManager::default());
        let agent_service = services.agents.clone();
        let factory_services = services.clone();
        let factory_lifetimes = service_lifetimes.clone();
        let service = StreamableHttpService::new(
            move || {
                Ok(KerminalMcpHandler::new(
                    factory_services.clone(),
                    factory_lifetimes.token(),
                ))
            },
            sessions.clone(),
            StreamableHttpServerConfig::default()
                .with_stateful_mode(true)
                .with_allowed_hosts(["localhost", "127.0.0.1", "::1"])
                .with_allowed_origins([allowed_origin.as_str()])
                .with_cancellation_token(cancellation.child_token()),
        );
        let router = Router::new()
            .nest_service("/mcp", service)
            .layer(middleware::from_fn_with_state(
                agent_service,
                inject_agent_session_scope,
            ))
            .layer(middleware::from_fn_with_state(
                allowed_origin,
                validate_mcp_origin,
            ));
        let shutdown = cancellation.clone();
        let task = runtime.spawn(async move {
            serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .map_err(|error| {
                    tracing::error!(%error, "MCP HTTP server stopped with an error");
                    AppError::infrastructure("MCP HTTP server failed")
                })
        });

        Ok(McpServerHandle {
            endpoint: format!("http://{local_address}/mcp"),
            cancellation,
            task: Some(task),
            sessions,
            service_lifetimes,
            shutdown_complete: false,
        })
    }
}

/// 先用唯一 listener origin 拦截不规范头，补齐 RMCP 对 malformed Origin 返回 400 的状态码差异。
async fn validate_mcp_origin(
    State(allowed_origin): State<String>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if is_allowed_origin_header(request.headers(), &allowed_origin) {
        return next.run(request).await;
    }
    StatusCode::FORBIDDEN.into_response()
}

/// 接受无 Origin 的原生客户端，只允许单个精确匹配的浏览器 Origin，拒绝重复和逗号合并值。
fn is_allowed_origin_header(headers: &HeaderMap, allowed_origin: &str) -> bool {
    let mut values = headers.get_all(ORIGIN).iter();
    match (values.next(), values.next()) {
        (None, None) => true,
        (Some(value), None) => value.as_bytes() == allowed_origin.as_bytes(),
        _ => false,
    }
}

/// 运行中的 MCP server 句柄；显式 shutdown 等待连接与 listener 有序停止。
pub struct McpServerHandle {
    endpoint: String,
    cancellation: CancellationToken,
    task: Option<JoinHandle<AppResult<()>>>,
    sessions: Arc<LocalSessionManager>,
    service_lifetimes: TaskTracker,
    shutdown_complete: bool,
}

impl McpServerHandle {
    /// 返回本次实际绑定的全局 MCP endpoint，包含系统分配的真实端口。
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// 返回该 listener 的取消信号副本，使外部资源 owner 可同步停止接入且仍保留本句柄等待 join。
    pub(crate) fn shutdown_signal(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// 为受管 Agent 构造只注入首选目标上下文的 endpoint，保持审批仍由 MCP host 负责。
    pub fn agent_endpoint(&self, agent_session_id: &AgentSessionId) -> String {
        format!("{}/agents/{}", self.endpoint, agent_session_id.as_str())
    }

    /// 便捷关闭入口使用短的本地预算；失败时保留句柄供组合根用共享 deadline 重试。
    pub async fn shutdown(&mut self) -> AppResult<()> {
        self.shutdown_until(Instant::now() + MCP_SHUTDOWN_TIMEOUT)
            .await
    }

    /// 同步停止接受新请求并通知所有 HTTP stream 开始清理。
    pub fn begin_shutdown(&self) {
        self.cancellation.cancel();
    }

    /// 在单一 deadline 内推进清理；超时不消费 listener/tracker owner，允许调用方继续重试。
    pub async fn shutdown_until(&mut self, deadline: Instant) -> AppResult<()> {
        self.begin_shutdown();
        if self.shutdown_complete {
            return Ok(());
        }
        let mut first_failure = None;

        if let Err(error) = close_sessions_once(&self.sessions, deadline).await {
            first_failure = Some(error);
        }

        if let Err(error) = join_server_task(&mut self.task, deadline).await {
            first_failure.get_or_insert(error);
        }

        if let Err(error) = close_sessions_once(&self.sessions, deadline).await {
            first_failure.get_or_insert(error);
        }

        // RMCP 2.1 在内部 spawn 的 worker 不返回 JoinHandle；service factory 为每个
        // handler 持有 token，只有 handler 真正析构后 tracker 才会完成。
        self.service_lifetimes.close();
        if let Err(error) = wait_service_lifetimes(&self.service_lifetimes, deadline).await {
            first_failure.get_or_insert(error);
        }

        if let Some(error) = first_failure {
            return Err(error);
        }
        self.shutdown_complete = true;
        Ok(())
    }
}

/// 关闭当前所有 RMCP session；并发 join 发送任务，避免关闭流程留下新的 detached task。
async fn close_sessions_once(
    sessions: &Arc<LocalSessionManager>,
    deadline: Instant,
) -> AppResult<()> {
    let session_ids = timeout_at(deadline, async {
        sessions
            .sessions
            .read()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|error| {
        AppError::infrastructure_with_source("MCP session cleanup timed out", error)
    })?;

    let mut closing = JoinSet::new();
    for session_id in session_ids {
        let sessions = Arc::clone(sessions);
        closing.spawn(async move { sessions.close_session(&session_id).await });
    }

    let mut first_failure = None;
    while !closing.is_empty() {
        match timeout_at(deadline, closing.join_next()).await {
            Ok(Some(Ok(Ok(())))) | Ok(None) => {}
            Ok(Some(Ok(Err(error)))) => {
                tracing::warn!(%error, "MCP session close request failed");
                first_failure
                    .get_or_insert_with(|| AppError::infrastructure("MCP session close failed"));
            }
            Ok(Some(Err(error))) => {
                tracing::warn!(%error, "MCP session close task failed");
                first_failure
                    .get_or_insert_with(|| AppError::infrastructure("MCP session close failed"));
            }
            Err(error) => {
                closing.abort_all();
                while closing.join_next().await.is_some() {}
                return Err(AppError::infrastructure_with_source(
                    "MCP session cleanup timed out",
                    error,
                ));
            }
        }
    }

    first_failure.map_or(Ok(()), Err)
}

/// 跨 await 只借用 owner 内的 listener 句柄；超时或外层取消都保留回收责任，完成后才清空。
///
/// @author kongweiguang
async fn join_server_task(
    owner: &mut Option<JoinHandle<AppResult<()>>>,
    deadline: Instant,
) -> AppResult<()> {
    let Some(task) = owner.as_mut() else {
        return Ok(());
    };
    let completed = match timeout_at(deadline, task).await {
        Ok(completed) => completed,
        Err(error) => {
            return Err(AppError::infrastructure_with_source(
                "MCP server shutdown timed out",
                error,
            ));
        }
    };
    *owner = None;
    match completed {
        Ok(result) => result,
        Err(error) => Err(AppError::infrastructure_with_source(
            "MCP server task failed",
            error,
        )),
    }
}

/// 等待所有 RMCP handler 生命周期令牌释放，确保内部 detached worker 不再持有服务对象。
async fn wait_service_lifetimes(tracker: &TaskTracker, deadline: Instant) -> AppResult<()> {
    timeout_at(deadline, tracker.wait()).await.map_err(|error| {
        tracing::warn!("RMCP service workers remained alive at shutdown deadline");
        AppError::infrastructure_with_source("MCP service shutdown timed out", error)
    })
}

/// 从 session-scoped URL 注入显式 Agent ID，避免 MCP handler 从 Tab 或客户端默认值猜测。
async fn inject_agent_session_scope(
    State(agent_service): State<Option<Arc<dyn AgentService>>>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if let Some(agent_id) = path.strip_prefix("/mcp/agents/") {
        if agent_id.is_empty() || agent_id.contains('/') || agent_id.contains('\\') {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
        let agent_id = match AgentSessionId::parse(agent_id) {
            Ok(agent_id) => agent_id,
            Err(_) => return axum::http::StatusCode::BAD_REQUEST.into_response(),
        };
        let Some(agent_service) = agent_service else {
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let session = match agent_service
            .get(agent_id.clone(), CancellationToken::new())
            .await
        {
            Ok(session) => session,
            Err(error) if error.public_code() == PublicErrorCode::NotFound => {
                return axum::http::StatusCode::NOT_FOUND.into_response();
            }
            Err(error) => {
                tracing::debug!(
                    code = error.public_code().as_str(),
                    "Agent endpoint session lookup failed"
                );
                return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
        };
        if session.session.status != kerminal_domain::agents::AgentSessionStatus::Active
            || !session.terminal.as_ref().is_some_and(|terminal| {
                terminal.lifecycle == kerminal_domain::sessions::SessionLifecycle::Running
            })
        {
            return axum::http::StatusCode::CONFLICT.into_response();
        }
        request.extensions_mut().insert(agent_id);
    }
    next.run(request).await
}

impl Drop for McpServerHandle {
    /// Drop 只在 owner 最终被释放时 abort listener；正常关闭会先通过异步 API join。
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// 保持旧版默认端口和有限冲突扫描，同时让显式端口 0 直接由系统分配。
async fn bind_listener(bind_address: SocketAddr) -> AppResult<TcpListener> {
    let requested_port = bind_address.port();
    let attempts = if requested_port == 0 {
        1
    } else {
        MCP_PORT_SCAN_LIMIT
    };
    for attempt in 0..attempts {
        let port = requested_port
            .checked_add(attempt)
            .ok_or_else(|| AppError::invalid("MCP port range exhausted"))?;
        let address = SocketAddr::new(bind_address.ip(), port);
        match TcpListener::bind(address).await {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == ErrorKind::AddrInUse && attempt + 1 < attempts => {}
            Err(error) => {
                tracing::error!(%error, %address, "failed to bind MCP loopback listener");
                return Err(AppError::infrastructure_with_source(
                    "MCP listener bind failed",
                    error,
                ));
            }
        }
    }
    Err(AppError::conflict(
        "MCP has no available loopback port in the configured range",
    ))
}

#[cfg(test)]
#[path = "../tests/unit/server.rs"]
mod tests;
