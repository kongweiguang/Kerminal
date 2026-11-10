//! 异步 Sidecar 运行时；标准输出只用于有界 NDJSON 协议。
//! @author kongweiguang

use std::{
    collections::HashMap,
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

pub use kerminal_plugin_protocol::ProgressParams;
use kerminal_plugin_protocol::{
    InitializeParams, MAX_PENDING_REQUESTS, MAX_RPC_BYTES, PROTOCOL_VERSION, RpcMessage, RpcPeer,
    error_code,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{Mutex, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

/// 错误只包含插件主动提供的安全文案，避免内部异常意外进入协议。
#[derive(Clone, Debug)]
pub struct Error {
    pub code: i32,
    pub message: String,
}
impl Error {
    /// 显式构造协议错误，调用者负责选择稳定错误码。
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
pub type Result<T> = std::result::Result<T, Error>;
type HandlerFuture = Pin<Box<dyn Future<Output = Result<Value>> + Send + 'static>>;
type Handler = Arc<dyn Fn(Context, Value, CancellationToken) -> HandlerFuture + Send + Sync>;

/// 克隆句柄共享连接和背压；请求取消只影响其自身。
#[derive(Clone)]
pub struct Context {
    shared: Arc<Shared>,
}
struct Shared {
    output: mpsc::Sender<Output>,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>,
    next: AtomicU64,
}
enum Output {
    Frame(Value),
    Final(Value),
}

/// 父请求退出时同步终止回调，避免关闭后后台任务继续访问插件资源。
struct AbortOnDrop(tokio::task::AbortHandle);
impl Drop for AbortOnDrop {
    /// 包装任务被取消时，中止仍在运行的用户回调。
    fn drop(&mut self) {
        self.0.abort();
    }
}
impl Context {
    /// 双向请求共享 64 个槽位；取消时释放槽位并通知宿主终止工作。
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let id = format!("p:{}", self.shared.next.fetch_add(1, Ordering::Relaxed));
        let (send, receive) = oneshot::channel();
        {
            let mut pending = self.shared.pending.lock().await;
            if pending.len() >= MAX_PENDING_REQUESTS {
                return Err(Error::new(error_code::OVERLOADED, "overloaded"));
            }
            pending.insert(id.clone(), send);
        }
        let frame = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let result = tokio::select! {
            _ = cancel.cancelled() => Err(Error::new(error_code::CANCELLED, "cancelled")),
            sent = self.shared.output.send(Output::Frame(frame)) => match sent {
                Err(_) => Err(Error::new(error_code::INTERNAL_ERROR, "connection closed")),
                Ok(()) => tokio::select! {
                    _ = cancel.cancelled() => Err(Error::new(error_code::CANCELLED, "cancelled")),
                    result = receive => result.unwrap_or_else(|_| Err(Error::new(error_code::INTERNAL_ERROR, "connection closed"))),
                },
            },
        };
        self.shared.pending.lock().await.remove(&id);
        if cancel.is_cancelled() {
            let _ = self.notify("$/cancel", json!({"id":id})).await;
        }
        result
    }

    /// 有界发送队列限制高频通知，避免插件无限累积内存。
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.shared
            .output
            .send(Output::Frame(
                json!({"jsonrpc":"2.0","method":method,"params":params}),
            ))
            .await
            .map_err(|_| Error::new(error_code::INTERNAL_ERROR, "connection closed"))
    }

    /// 直接使用唯一协议 DTO，避免便捷方法把 message/percent 包进宿主无法解释的 value 字段。
    pub async fn progress(&self, progress: ProgressParams) -> Result<()> {
        let params = serde_json::to_value(progress)
            .map_err(|_| Error::new(error_code::INVALID_PARAMS, "invalid progress"))?;
        self.notify("$/progress", params).await
    }

    /// 标准错误由监督器收集，绝不污染标准输出协议帧。
    pub fn log(&self, message: &str) {
        eprintln!("{message}");
    }
}

/// 路由在运行前冻结，SDK 接管握手、心跳、取消和退出。
pub struct Router {
    id: String,
    version: String,
    handlers: HashMap<String, Handler>,
}
impl Router {
    /// 身份由插件自身提供，防止错误程序盲目回显宿主期待的身份。
    pub fn new(id: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            handlers: HashMap::new(),
        }
    }

    /// 重复及协议保留方法一律拒绝，避免注册顺序改变内置行为。
    pub fn method<F, Fut>(&mut self, method: impl Into<String>, handler: F) -> Result<()>
    where
        F: Fn(Context, Value, CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        let method = method.into();
        if method.is_empty()
            || method.starts_with("$/")
            || matches!(method.as_str(), "initialize" | "initialized" | "shutdown")
            || self.handlers.contains_key(&method)
        {
            return Err(Error::new(
                error_code::INVALID_REQUEST,
                "invalid or duplicate method",
            ));
        }
        self.handlers.insert(
            method,
            Arc::new(move |cx, params, cancel| Box::pin(handler(cx, params, cancel))),
        );
        Ok(())
    }

    /// 正式入口复用可注入流的运行循环，便于测试相同的协议实现。
    pub async fn run(self) -> io::Result<()> {
        self.run_with_io(tokio::io::stdin(), tokio::io::stdout())
            .await
    }

    /// 单一读取者保留请求身份，业务任务并发执行以便心跳和取消及时抵达。
    pub async fn run_with_io<R, W>(self, input: R, output: W) -> io::Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (sender, mut receiver) = mpsc::channel::<Output>(MAX_PENDING_REQUESTS);
        let mut writer = tokio::spawn(async move {
            let mut output = output;
            while let Some(frame) = receiver.recv().await {
                let (value, final_frame) = match frame {
                    Output::Frame(value) => (value, false),
                    Output::Final(value) => (value, true),
                };
                let mut bytes = serde_json::to_vec(&value).map_err(io::Error::other)?;
                if bytes.len() + 1 > MAX_RPC_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "RPC frame too large",
                    ));
                }
                bytes.push(b'\n');
                output.write_all(&bytes).await?;
                output.flush().await?;
                if final_frame {
                    break;
                }
            }
            Ok::<(), io::Error>(())
        });
        let cx = Context {
            shared: Arc::new(Shared {
                output: sender,
                pending: Mutex::new(HashMap::new()),
                next: AtomicU64::new(1),
            }),
        };
        let active: Arc<Mutex<HashMap<String, CancellationToken>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut reader = BufReader::new(input);
        let mut initialized = false;
        let mut graceful = false;
        let mut tasks = tokio::task::JoinSet::new();
        let result = loop {
            // 写入端故障时立即终止读取，避免宿主关闭输出后仍无限等待输入。
            let bytes = tokio::select! {
                written = &mut writer => break match written {
                    Ok(Err(error)) => Err(error),
                    Ok(Ok(())) => Err(io::Error::other("writer closed")),
                    Err(error) => Err(io::Error::other(error)),
                },
                frame = read_frame(&mut reader) => match frame {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => break Ok(()),
                    Err(error) => break Err(error),
                },
            };
            let message: RpcMessage = match serde_json::from_slice(&bytes) {
                Ok(message) => message,
                Err(error) => break Err(io::Error::new(io::ErrorKind::InvalidData, error)),
            };
            match message {
                RpcMessage::Response(response) => {
                    if response.validate().is_err() {
                        break Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid response",
                        ));
                    }
                    if let Some(id) = response.id
                        && let Some(waiter) = cx.shared.pending.lock().await.remove(&id)
                    {
                        let result = match response.error {
                            Some(error) => Err(Error::new(error.code, error.message)),
                            None => Ok(response.result.unwrap_or(Value::Null)),
                        };
                        let _ = waiter.send(result);
                    }
                }
                RpcMessage::Notification(notification) => {
                    if notification.validate().is_err() {
                        break Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid notification",
                        ));
                    }
                    if notification.method == "$/cancel" {
                        if let Some(id) = notification
                            .params
                            .as_ref()
                            .and_then(|value| value.get("id"))
                            .and_then(Value::as_str)
                            && let Some(token) = active.lock().await.get(id)
                        {
                            token.cancel();
                        }
                    } else if initialized && notification.method != "initialized" {
                        while tasks.try_join_next().is_some() {}
                        if tasks.len() >= MAX_PENDING_REQUESTS {
                            continue;
                        }
                        if let Some(handler) = self.handlers.get(&notification.method) {
                            let handler = handler.clone();
                            let cx = cx.clone();
                            let params = notification.params.unwrap_or_else(|| json!({}));
                            tasks.spawn(async move {
                                let _ = handler(cx, params, CancellationToken::new()).await;
                            });
                        }
                    }
                }
                RpcMessage::Request(request) => {
                    if request.validate(RpcPeer::Host).is_err() {
                        break Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid request",
                        ));
                    }
                    let id = request.id;
                    let params = request.params.unwrap_or_else(|| json!({}));
                    if request.method == "initialize" && !initialized {
                        let valid = serde_json::from_value::<InitializeParams>(params)
                            .ok()
                            .is_some_and(|params| {
                                params.protocol == PROTOCOL_VERSION
                                    && params.plugin.id == self.id
                                    && params.plugin.version == self.version
                            });
                        if !valid {
                            break Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "plugin identity mismatch",
                            ));
                        }
                        initialized = true;
                        if cx.shared.output.send(Output::Frame(json!({"jsonrpc":"2.0","id":id,"result":{"protocol":PROTOCOL_VERSION,"plugin":{"id":self.id,"version":self.version}}}))).await.is_err() { break Err(io::Error::other("writer closed")); }
                    } else if initialized && request.method == "$/ping" {
                        if cx
                            .shared
                            .output
                            .send(Output::Frame(json!({"jsonrpc":"2.0","id":id,"result":{}})))
                            .await
                            .is_err()
                        {
                            break Err(io::Error::other("writer closed"));
                        }
                    } else if initialized && request.method == "shutdown" {
                        for token in active.lock().await.values() {
                            token.cancel();
                        }
                        tasks.abort_all();
                        while tasks.join_next().await.is_some() {}
                        if cx
                            .shared
                            .output
                            .send(Output::Final(json!({"jsonrpc":"2.0","id":id,"result":{}})))
                            .await
                            .is_err()
                        {
                            break Err(io::Error::other("writer closed"));
                        }
                        graceful = true;
                        break Ok(());
                    } else if !initialized {
                        break Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "handshake required",
                        ));
                    } else {
                        let mut requests = active.lock().await;
                        if requests.contains_key(&id) {
                            break Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "duplicate request",
                            ));
                        }
                        if requests.len() >= MAX_PENDING_REQUESTS {
                            drop(requests);
                            if cx.shared.output.send(Output::Frame(json!({"jsonrpc":"2.0","id":id,"error":{"code":error_code::OVERLOADED,"message":"overloaded"}}))).await.is_err() { break Err(io::Error::other("writer closed")); }
                            continue;
                        }
                        let token = CancellationToken::new();
                        requests.insert(id.clone(), token.clone());
                        drop(requests);
                        let handler = self.handlers.get(&request.method).cloned();
                        let cx = cx.clone();
                        let active = active.clone();
                        tasks.spawn(async move {
                            // 单独监督插件回调，panic 也必须释放请求槽并返回受控错误。
                            let result = match handler {
                                Some(handler) => {
                                    let child_cx = cx.clone();
                                    let child = tokio::spawn(async move {
                                        handler(child_cx, params, token).await
                                    });
                                    let abort = child.abort_handle();
                                    let guard = AbortOnDrop(abort);
                                    let result = child.await;
                                    drop(guard);
                                    match result {
                                        Ok(result) => result,
                                        Err(_) => Err(Error::new(error_code::INTERNAL_ERROR, "handler failed")),
                                    }
                                },
                                None => Err(Error::new(error_code::METHOD_NOT_FOUND, "method not found")),
                            };
                            let response = match result { Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}), Err(error) => json!({"jsonrpc":"2.0","id":id,"error":{"code":error.code,"message":error.message}}) };
                            // 回执可见前释放槽位，避免收到取消回执后立即重试仍误报过载。
                            active.lock().await.remove(&id);
                            let _ = cx.shared.output.send(Output::Frame(response)).await;
                        });
                    }
                }
            }
            while tasks.try_join_next().is_some() {}
        };
        for token in active.lock().await.values() {
            token.cancel();
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        drop(cx);
        if graceful {
            writer.await.map_err(io::Error::other)??;
        } else if !writer.is_finished() {
            writer.abort();
            let _ = writer.await;
        }
        result
    }
}

/// 在复制之前检查上限，未终止或过长的帧不会无限扩张内存。
async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete RPC frame",
                ))
            };
        }
        let end = available.iter().position(|byte| *byte == b'\n');
        let count = end.unwrap_or(available.len());
        if frame.len() + count + 1 > MAX_RPC_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RPC frame too large",
            ));
        }
        frame.extend_from_slice(&available[..count]);
        reader.consume(count + usize::from(end.is_some()));
        if end.is_some() {
            return Ok(Some(frame));
        }
    }
}

/// 集中登记路由并传播重复方法错误，保留正常 Rust 闭包签名。
#[macro_export]
macro_rules! routes {
    ($router:expr, $( $name:expr => $handler:expr ),+ $(,)?) => {{
        (|| -> $crate::Result<()> { $( $router.method($name, $handler)?; )+ Ok(()) })()
    }};
}
