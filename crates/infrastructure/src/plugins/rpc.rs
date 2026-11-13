//! 有界的双向 JSON-RPC 连接；一个实例只服务一个进程代次。
//! @author kongweiguang

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use kerminal_application::{
    AppError, AppResult,
    plugin_ports::{
        PluginBusinessError, PluginHostError, PluginHostRouter, PluginResponseObserver,
    },
};
use kerminal_domain::plugins::PluginId;
use kerminal_plugin_protocol::{
    MAX_PENDING_REQUESTS, MAX_RPC_BYTES, RpcNotification, RpcPeer, RpcRequest, RpcResponse,
    error_code,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{Semaphore, mpsc, oneshot, watch},
    time::{Instant as TokioInstant, sleep_until, timeout_at},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

pub(crate) type RouterSlot = Arc<Mutex<Option<Weak<dyn PluginHostRouter>>>>;

struct Pending {
    response: oneshot::Sender<AppResult<Value>>,
    progress: watch::Sender<bool>,
    observer: Option<PluginResponseObserver>,
    cancellation: CancellationToken,
    expires: Instant,
    maximum: Instant,
}

struct OutgoingFrame {
    bytes: Vec<u8>,
    cancellation: Option<CancellationToken>,
}

struct Shared {
    plugin: PluginId,
    generation: u64,
    router: RouterSlot,
    pending: Mutex<HashMap<String, Pending>>,
    incoming: Mutex<HashMap<String, CancellationToken>>,
    outgoing_limit: Arc<Semaphore>,
    incoming_limit: Arc<Semaphore>,
    output: mpsc::Sender<OutgoingFrame>,
    priority: mpsc::Sender<Vec<u8>>,
    notifications: mpsc::Sender<(String, Value)>,
    closed: CancellationToken,
    tasks: TaskTracker,
    next_id: AtomicU64,
}

/// 克隆句柄共享同一组配额和请求表，避免多个窗口绕过背压。
#[derive(Clone)]
pub struct RpcConnection {
    inner: Arc<Shared>,
}

struct RequestGuard {
    inner: Arc<Shared>,
    id: String,
    queued: CancellationToken,
    completed: bool,
}

impl Drop for RequestGuard {
    /// 调用 future 被丢弃时也撤销请求表并尽力通知对端，绝不重放副作用。
    fn drop(&mut self) {
        if let Ok(mut pending) = self.inner.pending.lock() {
            pending.remove(&self.id);
        }
        if !self.completed {
            self.queued.cancel();
            if let Ok(frame) =
                encode(&json!({"jsonrpc":"2.0", "method":"$/cancel", "params":{"id":self.id}}))
            {
                let _ = self.inner.priority.try_send(frame);
            }
        }
    }
}

impl RpcConnection {
    /// 单独提供流构造入口，使真实管道和内存双工流遵守完全相同的协议实现。
    pub fn from_streams<R, W>(
        plugin: PluginId,
        generation: u64,
        reader: R,
        writer: W,
        router: Option<Weak<dyn PluginHostRouter>>,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::new(
            plugin,
            generation,
            reader,
            writer,
            Arc::new(Mutex::new(router)),
        )
    }

    /// reader/writer 由受管子进程拥有，连接本身不创建独立运行时。
    pub(crate) fn new<R, W>(
        plugin: PluginId,
        generation: u64,
        reader: R,
        writer: W,
        router: RouterSlot,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (output, outgoing) = mpsc::channel(MAX_PENDING_REQUESTS);
        let (priority, urgent) = mpsc::channel(MAX_PENDING_REQUESTS);
        let (notifications, notification_queue) = mpsc::channel(MAX_PENDING_REQUESTS);
        let inner = Arc::new(Shared {
            plugin,
            generation,
            router,
            pending: Mutex::new(HashMap::new()),
            incoming: Mutex::new(HashMap::new()),
            outgoing_limit: Arc::new(Semaphore::new(MAX_PENDING_REQUESTS)),
            incoming_limit: Arc::new(Semaphore::new(MAX_PENDING_REQUESTS)),
            output,
            priority,
            notifications,
            closed: CancellationToken::new(),
            tasks: TaskTracker::new(),
            next_id: AtomicU64::new(1),
        });
        inner
            .tasks
            .spawn(write_frames(Arc::clone(&inner), writer, outgoing, urgent));
        inner.tasks.spawn(read_frames(Arc::clone(&inner), reader));
        inner.tasks.spawn(dispatch_notifications(
            Arc::clone(&inner),
            notification_queue,
        ));
        Self { inner }
    }

    /// 常规请求不携带观察器，适用于不改变视图缓存的协议方法。
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> AppResult<Value> {
        self.request_inner(method, params, deadline, cancellation, None)
            .await
    }

    /// 视图回执在读取后续帧之前同步提交，避免后到更新抢先于回执。
    pub async fn request_observed(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        cancellation: CancellationToken,
        observer: PluginResponseObserver,
    ) -> AppResult<Value> {
        self.request_inner(method, params, deadline, cancellation, Some(observer))
            .await
    }

    /// 调用者的绝对截止时间是硬上限；进度只能延长方法默认等待且最多 300 秒。
    async fn request_inner(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        cancellation: CancellationToken,
        observer: Option<PluginResponseObserver>,
    ) -> AppResult<Value> {
        if self.inner.closed.is_cancelled() {
            return Err(stale());
        }
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件请求已取消"));
        }
        if Instant::now() >= deadline {
            return Err(AppError::timeout("插件请求"));
        }
        let _permit = Arc::clone(&self.inner.outgoing_limit)
            .try_acquire_owned()
            .map_err(|_| AppError::conflict("插件请求过多"))?;
        let sequence = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        if sequence == u64::MAX {
            self.inner.closed.cancel();
            return Err(stale());
        }
        let id = format!("h:{sequence}");
        let request = RpcRequest::new(id.clone(), method, params);
        request
            .validate(RpcPeer::Host)
            .map_err(|_| AppError::invalid("插件请求信封"))?;
        let frame = encode(&request)?;
        let (response, mut received) = oneshot::channel();
        let (progress, mut progress_rx) = watch::channel(false);
        let now = Instant::now();
        let maximum = deadline.min(now + Duration::from_secs(300));
        let mut expires = deadline.min(now + method_timeout(method));
        self.inner.pending.lock().map_err(|_| stale())?.insert(
            id.clone(),
            Pending {
                response,
                progress,
                observer,
                cancellation: cancellation.clone(),
                expires,
                maximum,
            },
        );
        let mut guard = RequestGuard {
            inner: Arc::clone(&self.inner),
            id,
            queued: CancellationToken::new(),
            completed: true,
        };
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件请求已取消"));
        }
        if Instant::now() >= deadline {
            return Err(AppError::timeout("插件请求"));
        }
        self.inner
            .output
            .try_send(OutgoingFrame {
                bytes: frame,
                cancellation: Some(guard.queued.clone()),
            })
            .map_err(|_| AppError::conflict("插件发送队列已满"))?;
        guard.completed = false;
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(AppError::cancelled("插件请求已取消")),
                _ = self.inner.closed.cancelled() => return Err(stale()),
                response = &mut received => {
                    guard.completed = true;
                    return response.map_err(|_| stale())?;
                }
                changed = progress_rx.changed() => {
                    if changed.is_ok() && *progress_rx.borrow_and_update() {
                        expires = maximum;
                    }
                }
                _ = sleep_until(TokioInstant::from_std(expires)) => return Err(AppError::timeout("插件请求")),
            }
        }
    }

    /// 通知也受队列和绝对截止时间约束，不假定写入队列等于插件已处理。
    pub async fn notify(&self, method: &str, params: Value, deadline: Instant) -> AppResult<()> {
        let notification = RpcNotification::new(method, params);
        notification
            .validate()
            .map_err(|_| AppError::invalid("插件通知信封"))?;
        let frame = encode(&notification)?;
        if self.inner.closed.is_cancelled() {
            return Err(stale());
        }
        let permit = tokio::select! {
            biased;
            _ = sleep_until(TokioInstant::from_std(deadline)) => return Err(AppError::timeout("插件通知")),
            _ = self.inner.closed.cancelled() => return Err(stale()),
            permit = self.inner.output.reserve() => permit.map_err(|_| stale())?,
        };
        if Instant::now() >= deadline {
            return Err(AppError::timeout("插件通知"));
        }
        permit.send(OutgoingFrame {
            bytes: frame,
            cancellation: None,
        });
        Ok(())
    }

    /// 读取进程管道或协议故障时，监督器据此回收整个进程树。
    pub async fn closed(&self) {
        self.inner.closed.cancelled().await;
    }

    /// 主动终止所有等待和管道任务；旧代次的任何迟到回复都不会投递。
    pub fn close(&self) {
        self.inner.closed.cancel();
        self.inner.tasks.close();
        if let Ok(incoming) = self.inner.incoming.lock() {
            for cancellation in incoming.values() {
                cancellation.cancel();
            }
        }
    }

    /// 关闭后等待全部管道与宿主回调；超时保留跟踪器供调用方继续回收。
    pub async fn close_until(&self, deadline: Instant) -> AppResult<()> {
        self.close();
        timeout_at(TokioInstant::from_std(deadline), self.inner.tasks.wait())
            .await
            .map_err(|_| AppError::timeout("回收插件 RPC 任务"))
    }
}

/// 默认等待与方法语义有关，取消、心跳和握手仍服从外部硬截止时间。
fn method_timeout(method: &str) -> Duration {
    match method {
        "initialize" => Duration::from_secs(10),
        "view.open" | "view.fetch" | "$/ping" => Duration::from_secs(5),
        "host.test" => Duration::from_secs(60),
        _ => Duration::from_secs(30),
    }
}

/// 单帧长度包含换行符，避免接收端分配超过协议上限的缓冲区。
fn encode(value: &impl serde::Serialize) -> AppResult<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| AppError::invalid("插件协议编码"))?;
    if bytes.len() + 1 > MAX_RPC_BYTES {
        return Err(AppError::invalid("插件协议帧过大"));
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// 单写入者保持帧完整；响应和取消优先，避免对端请求被普通消息饿死。
async fn write_frames<W: AsyncWrite + Unpin>(
    inner: Arc<Shared>,
    mut writer: W,
    mut output: mpsc::Receiver<OutgoingFrame>,
    mut priority: mpsc::Receiver<Vec<u8>>,
) {
    loop {
        let frame = tokio::select! {
            biased;
            _ = inner.closed.cancelled() => break,
            frame = priority.recv() => frame.map(|bytes| OutgoingFrame { bytes, cancellation: None }),
            frame = output.recv() => frame,
        };
        let Some(frame) = frame else { break };
        if frame
            .cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            continue;
        }
        let written = tokio::select! {
            _ = inner.closed.cancelled() => break,
            result = async {
                writer.write_all(&frame.bytes).await?;
                writer.flush().await
            } => result,
        };
        if written.is_err() {
            break;
        }
    }
    inner.closed.cancel();
}

/// 逐帧限长并累计协议错误；坏帧不能使读取器无限增长或误投递回执。
async fn read_frames<R: AsyncRead + Unpin>(inner: Arc<Shared>, reader: R) {
    let mut reader = BufReader::new(reader);
    let mut errors = 0_u8;
    loop {
        let mut bytes = Vec::new();
        let mut limited = (&mut reader).take((MAX_RPC_BYTES + 1) as u64);
        let read = tokio::select! {
            _ = inner.closed.cancelled() => break,
            result = limited.read_until(b'\n', &mut bytes) => result,
        };
        match read {
            Ok(0) | Err(_) => break,
            Ok(size) if size <= MAX_RPC_BYTES && bytes.last() == Some(&b'\n') => {}
            _ => break,
        }
        let valid = match serde_json::from_slice::<Value>(&bytes) {
            Ok(frame) => dispatch_frame(&inner, frame),
            Err(_) => false,
        };
        if !valid {
            errors = errors.saturating_add(1);
            if errors >= 3 {
                break;
            }
        }
    }
    inner.closed.cancel();
}

/// 先区分请求与回执，再校验方向和信封；迟到的合法回执只丢弃。
fn dispatch_frame(inner: &Arc<Shared>, frame: Value) -> bool {
    if frame.get("method").is_some() {
        if frame.get("id").is_some() {
            let Ok(request) = serde_json::from_value::<RpcRequest>(frame) else {
                return false;
            };
            if request.validate(RpcPeer::Plugin).is_err() {
                return false;
            }
            if request.method == "view.update" {
                dispatch_ordered_update_request(inner, request);
            } else {
                dispatch_request(inner, request);
            }
            return true;
        }
        let Ok(notification) = serde_json::from_value::<RpcNotification>(frame) else {
            return false;
        };
        if notification.validate().is_err() {
            return false;
        }
        // `$/executionTimeout` 是已移除脚本宿主遗留的保留通知；任何进程发送它都属于
        // 未实现语义，继续按协议错误拒绝，避免旧脚本帧被误当作可用能力。
        if notification.method == "$/executionTimeout" {
            return false;
        }
        dispatch_notification(inner, notification);
        return true;
    }
    let Ok(response) = serde_json::from_value::<RpcResponse>(frame) else {
        return false;
    };
    if response.validate().is_err() || response.id.as_ref().is_some_and(|id| !id.starts_with("h:"))
    {
        return false;
    }
    if let Some(id) = response.id {
        let entry = inner
            .pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&id));
        if let Some(entry) = entry {
            let result = match (response.result, response.error) {
                _ if entry.cancellation.is_cancelled() => {
                    Err(AppError::cancelled("插件请求已取消"))
                }
                _ if inner.closed.is_cancelled() => Err(stale()),
                _ if Instant::now() >= entry.expires => Err(AppError::timeout("插件请求")),
                (Some(value), None) => match entry.observer {
                    Some(observer) => observer(&value).map(|()| value),
                    None => Ok(value),
                },
                (None, Some(error)) => Err(map_remote_error(PluginHostError {
                    code: error.code,
                    message: error.message,
                })),
                _ => return false,
            };
            let _ = entry.response.send(result);
        }
    }
    true
}

/// 视图更新只执行同步内存提交，使它与相邻回执遵循同一管道顺序。
fn dispatch_ordered_update_request(inner: &Arc<Shared>, request: RpcRequest) {
    let Ok(_permit) = Arc::clone(&inner.incoming_limit).try_acquire_owned() else {
        send_error(inner, &request.id, error_code::OVERLOADED, "overloaded");
        return;
    };
    if inner
        .incoming
        .lock()
        .map_or(true, |incoming| incoming.contains_key(&request.id))
    {
        send_error(
            inner,
            &request.id,
            error_code::INVALID_REQUEST,
            "duplicate request id",
        );
        return;
    }
    let result = ordered_view_update(inner, request.params.unwrap_or_else(|| json!({})));
    let response = match result {
        Ok(value) => json!({"jsonrpc":"2.0","id":request.id,"result":value}),
        Err(error) => {
            json!({"jsonrpc":"2.0","id":request.id,"error":{"code":error.code,"message":error.message.chars().take(200).collect::<String>()}})
        }
    };
    if encode(&response)
        .ok()
        .and_then(|frame| inner.priority.try_send(frame).ok())
        .is_none()
    {
        inner.closed.cancel();
    }
}

/// 仅使用宿主安装的同步视图路由，失效路由不能切换到其他代次。
fn ordered_view_update(inner: &Shared, params: Value) -> Result<Value, PluginHostError> {
    let router = inner
        .router
        .lock()
        .ok()
        .and_then(|router| router.as_ref().and_then(Weak::upgrade));
    match router {
        Some(router) => router.ordered_view_update(&inner.plugin, inner.generation, params),
        None => Err(PluginHostError {
            code: error_code::STALE,
            message: "stale".into(),
        }),
    }
}

/// 插件请求数达到上限时立即回复 overloaded，不能阻塞读循环。
fn dispatch_request(inner: &Arc<Shared>, request: RpcRequest) {
    let Ok(permit) = Arc::clone(&inner.incoming_limit).try_acquire_owned() else {
        send_error(inner, &request.id, error_code::OVERLOADED, "overloaded");
        return;
    };
    let cancellation = inner.closed.child_token();
    let duplicate = match inner.incoming.lock() {
        Ok(mut incoming) => {
            if incoming.contains_key(&request.id) {
                true
            } else {
                incoming.insert(request.id.clone(), cancellation.clone());
                false
            }
        }
        Err(_) => true,
    };
    if duplicate {
        send_error(
            inner,
            &request.id,
            error_code::INVALID_REQUEST,
            "duplicate request id",
        );
        return;
    }
    let tasks = inner.tasks.clone();
    let inner = Arc::clone(inner);
    tasks.spawn(async move {
        let _permit = permit;
        let result = route(
            &inner,
            &request.method,
            request.params.unwrap_or_else(|| json!({})),
            cancellation,
        )
        .await;
        if let Ok(mut incoming) = inner.incoming.lock() {
            incoming.remove(&request.id);
        }
        let response = match result {
            Ok(value) => json!({"jsonrpc":"2.0","id":request.id,"result":value}),
            Err(error) => {
                json!({"jsonrpc":"2.0","id":request.id,"error":{"code":error.code,"message":error.message.chars().take(200).collect::<String>()}})
            }
        };
        if let Ok(frame) = encode(&response) {
            if inner.priority.send(frame).await.is_err() {
                inner.closed.cancel();
            }
        } else {
            inner.closed.cancel();
        }
    });
}

/// 取消只命中本代次的请求；进度只延长 token 对应的活动请求。
fn dispatch_notification(inner: &Arc<Shared>, notification: RpcNotification) {
    let params = notification.params.unwrap_or_else(|| json!({}));
    if notification.method == "$/cancel" {
        if let Some(id) = params.get("id").and_then(Value::as_str)
            && let Ok(incoming) = inner.incoming.lock()
            && let Some(token) = incoming.get(id)
        {
            token.cancel();
        }
        return;
    }
    if notification.method == "$/progress"
        && let Some(id) = params.get("token").and_then(Value::as_str)
        && let Ok(mut pending) = inner.pending.lock()
        && let Some(entry) = pending.get_mut(id)
        && Instant::now() < entry.expires
    {
        entry.expires = entry.maximum;
        let _ = entry.progress.send(true);
    }
    if notification.method == "view.update" {
        let _ = ordered_view_update(inner, params);
        return;
    }
    if inner
        .notifications
        .try_send((notification.method, params))
        .is_err()
    {
        inner.closed.cancel();
    }
}

/// 其他通知由单个有界消费者顺序处理，不能因 task 调度而倒序或无限积压。
async fn dispatch_notifications(
    inner: Arc<Shared>,
    mut notifications: mpsc::Receiver<(String, Value)>,
) {
    loop {
        let next = tokio::select! {
            _ = inner.closed.cancelled() => break,
            value = notifications.recv() => value,
        };
        let Some((method, params)) = next else { break };
        let token = inner.closed.child_token();
        // 回调收到取消后仍可能清理资源，必须等它返回才能承认通知任务已回收。
        let _ = route(&inner, &method, params, token).await;
    }
}

/// 路由器消失时明确返回 stale，绝不切换到其他插件或实例。
async fn route(
    inner: &Shared,
    method: &str,
    params: Value,
    token: CancellationToken,
) -> Result<Value, PluginHostError> {
    let router = inner
        .router
        .lock()
        .ok()
        .and_then(|router| router.as_ref().and_then(Weak::upgrade));
    match router {
        Some(router) => {
            router
                .route(&inner.plugin, inner.generation, method, params, token)
                .await
        }
        None => Err(PluginHostError {
            code: error_code::STALE,
            message: "stale".into(),
        }),
    }
}

/// 无法排入错误回执表示连接无法保持协议一致，应交监督器回收。
fn send_error(inner: &Shared, id: &str, code: i32, message: &str) {
    let result = encode(&json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
        .ok()
        .and_then(|frame| inner.priority.try_send(frame).ok());
    if result.is_none() {
        inner.closed.cancel();
    }
}

/// 只映射协议分类，不把插件提供的错误内容写入宿主诊断。
fn map_remote_error(error: PluginHostError) -> AppError {
    match error.code {
        error_code::CANCELLED => AppError::cancelled("插件请求已取消"),
        error_code::OVERLOADED => AppError::conflict("插件请求过多"),
        error_code::STALE => stale(),
        error_code::TIMEOUT => AppError::timeout("插件请求"),
        error_code::PERMISSION_DENIED => AppError::unsupported("插件权限不足"),
        code if (error_code::PLUGIN_BUSINESS_MIN..=error_code::PLUGIN_BUSINESS_MAX)
            .contains(&code) =>
        {
            match PluginBusinessError::new(code, &error.message) {
                Some(source) => AppError::infrastructure_with_source("插件请求失败", source),
                None => AppError::infrastructure("插件请求失败"),
            }
        }
        _ => AppError::infrastructure("插件请求失败"),
    }
}

/// 连接中断始终属于旧代次，不能推断副作用是否执行。
fn stale() -> AppError {
    AppError::conflict("插件进程代次已失效")
}
