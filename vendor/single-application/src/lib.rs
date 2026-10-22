use interprocess::local_socket::{GenericNamespaced, ListenerOptions, ToNsName};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    io::ErrorKind,
    marker::PhantomData,
    ops::{Deref, DerefMut},
};

const MAX_TOKIO_MESSAGE_BYTES: usize = 128 * 1024;

#[cfg(feature = "tokio")]
use std::sync::Arc;

#[cfg(not(feature = "tokio"))]
use std::sync::mpsc::Receiver;

#[cfg(feature = "tokio")]
use tokio::sync::mpsc::Receiver;

/// Primary-instance handle,can receive messages from the Secondary instance.
pub struct PrimaryInstance<T>
where
    T: Send + Serialize + DeserializeOwned + 'static,
{
    receiver: Receiver<T>,
    #[cfg(feature = "tokio")]
    shutdown: PrimaryInstanceShutdownHandle,
}

/// Tokio 单实例监听器的关闭错误；超时不消耗任务句柄，调用方可以续等。
#[cfg(feature = "tokio")]
#[derive(Debug)]
pub enum PrimaryInstanceShutdownError {
    /// 绝对截止时间到达，监听器任务仍由原 owner 保留。
    TimedOut,
    /// 监听器任务异常退出；任务结束时其 listener 已释放。
    Join(tokio::task::JoinError),
}

/// Secondary 消息失败分类；Display 不包含 instance ID、参数或 IPC 消息正文。
#[derive(Debug)]
pub enum SecondaryInstanceSendError {
    Name(std::io::Error),
    Connect(std::io::Error),
    Serialize(serde_json::Error),
    Oversized,
    Write(std::io::Error),
}

impl std::fmt::Display for SecondaryInstanceSendError {
    /// 为旧 API 日志和 UI 错误提供固定说明，不泄露单实例标识或启动参数。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Name(_) => "无法构造单实例本地套接字名称",
            Self::Connect(_) => "无法连接 Kerminal 主实例",
            Self::Serialize(_) => "单实例消息编码失败",
            Self::Oversized => "单实例消息超过接收上限",
            Self::Write(_) => "单实例消息发送失败",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for SecondaryInstanceSendError {
    /// 原始错误仅作为 source 留给受控诊断，不纳入稳定展示文本。
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Name(error) | Self::Connect(error) | Self::Write(error) => Some(error),
            Self::Serialize(error) => Some(error),
            Self::Oversized => None,
        }
    }
}

#[cfg(feature = "tokio")]
impl std::fmt::Display for PrimaryInstanceShutdownError {
    /// 使用不含单实例标识或 IPC 消息的安全说明，避免错误文本泄露启动参数。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut => formatter.write_str("等待单实例监听器关闭超时"),
            Self::Join(_) => formatter.write_str("单实例监听器任务异常结束"),
        }
    }
}

#[cfg(feature = "tokio")]
impl std::error::Error for PrimaryInstanceShutdownError {
    /// Join 错误仅保留作内部诊断来源，不将原始 IPC 内容纳入公开错误。
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TimedOut => None,
            Self::Join(error) => Some(error),
        }
    }
}

/// 持有 package listener task 的可克隆关闭控制；deadline timeout 后保留同一 JoinHandle。
#[cfg(feature = "tokio")]
#[derive(Clone)]
pub struct PrimaryInstanceShutdownHandle {
    cancellation: tokio::sync::watch::Sender<bool>,
    listener_task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

#[cfg(feature = "tokio")]
impl PrimaryInstanceShutdownHandle {
    /// 请求唯一 accept loop 停止并在绝对截止时间内 join，超时可对同一 owner 继续等待。
    pub async fn shutdown_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<(), PrimaryInstanceShutdownError> {
        self.cancellation.send_replace(true);
        let mut task_owner = tokio::time::timeout_at(deadline, self.listener_task.lock())
            .await
            .map_err(|_| PrimaryInstanceShutdownError::TimedOut)?;
        let join_result = if let Some(task) = task_owner.as_mut() {
            tokio::time::timeout_at(deadline, task).await
        } else {
            return Ok(());
        };
        match join_result {
            Ok(Ok(())) => {
                task_owner.take();
                Ok(())
            }
            Ok(Err(error)) => {
                task_owner.take();
                Err(PrimaryInstanceShutdownError::Join(error))
            }
            Err(_) => Err(PrimaryInstanceShutdownError::TimedOut),
        }
    }
}

#[cfg(feature = "tokio")]
impl<T> PrimaryInstance<T>
where
    T: Send + Serialize + DeserializeOwned + 'static,
{
    /// 给宿主保留 listener task 的独立 shutdown owner，消息 consumer 仍按原 receiver API 工作。
    pub fn shutdown_handle(&self) -> PrimaryInstanceShutdownHandle {
        self.shutdown.clone()
    }
}

#[cfg(feature = "tokio")]
impl<T> Drop for PrimaryInstance<T>
where
    T: Send + Serialize + DeserializeOwned + 'static,
{
    /// 组合根初始化失败或异常退出时，drop package 主句柄也会请求 accept loop 收尾。
    fn drop(&mut self) {
        self.shutdown.cancellation.send_replace(true);
    }
}

// Kerminal listener-lifecycle patch, @author kongweiguang.

impl<T> Deref for PrimaryInstance<T>
where
    T: Send + Serialize + DeserializeOwned + 'static,
{
    type Target = Receiver<T>;

    fn deref(&self) -> &Self::Target {
        &self.receiver
    }
}

impl<T> DerefMut for PrimaryInstance<T>
where
    T: Send + Serialize + DeserializeOwned + 'static,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.receiver
    }
}

/// Secondary-instance handle, can send messages to the primary instance
pub struct SecondaryInstance<T>
where
    T: Send + Serialize + DeserializeOwned + 'static,
{
    id: String,
    _phantom: PhantomData<T>,
}

impl<T> SecondaryInstance<T>
where
    T: Send + Serialize + DeserializeOwned + 'static,
{
    /// 同步发送并保留失败分类；只有成功写入本地 socket，不表示主实例已处理业务。
    #[cfg(not(feature = "tokio"))]
    pub fn try_send_message(&self, message: T) -> Result<(), SecondaryInstanceSendError> {
        use interprocess::local_socket::traits::Stream as _;

        let id = <String as AsRef<str>>::as_ref(&self.id);
        let name = id
            .to_ns_name::<GenericNamespaced>()
            .map_err(SecondaryInstanceSendError::Name)?;
        let stream = interprocess::local_socket::Stream::connect(name)
            .map_err(SecondaryInstanceSendError::Connect)?;
        serde_json::to_writer(stream, &message).map_err(SecondaryInstanceSendError::Serialize)
    }

    /// 保留同步兼容 API；新桌面入口应使用 try_send_message 处理错误。
    /// Sends a message to the primary instance.
    #[cfg(not(feature = "tokio"))]
    pub fn send_message(&self, message: T) {
        if self.try_send_message(message).is_err() {
            tracing::error!("Single-instance message could not be sent");
        }
    }

    /// 异步发送并保留失败分类；没有应用层 ACK，因此成功只代表写入完成。
    #[cfg(feature = "tokio")]
    pub async fn try_send_message(&self, message: T) -> Result<(), SecondaryInstanceSendError> {
        use interprocess::local_socket::traits::tokio::Stream;
        use tokio::io::AsyncWriteExt;

        let id = <String as AsRef<str>>::as_ref(&self.id);
        let name = id
            .to_ns_name::<GenericNamespaced>()
            .map_err(SecondaryInstanceSendError::Name)?;
        let encoded =
            serde_json::to_vec(&message).map_err(SecondaryInstanceSendError::Serialize)?;
        if encoded.len() > MAX_TOKIO_MESSAGE_BYTES {
            return Err(SecondaryInstanceSendError::Oversized);
        }
        let mut stream = interprocess::local_socket::tokio::Stream::connect(name)
            .await
            .map_err(SecondaryInstanceSendError::Connect)?;
        stream
            .write_all(&encoded)
            .await
            .map_err(SecondaryInstanceSendError::Write)
    }

    /// Sends a message to the primary instance.
    #[cfg(feature = "tokio")]
    pub async fn send_message(&self, message: T) {
        if self.try_send_message(message).await.is_err() {
            tracing::error!("Single-instance message could not be sent");
        }
    }
}

/// Two-state handle for a single-instance application.
pub enum SingleApplication<T = String>
where
    T: Send + Serialize + DeserializeOwned + 'static,
{
    /// Primary instance
    PrimaryInstance(PrimaryInstance<T>),
    /// Secondary instance
    SecondaryInstance(SecondaryInstance<T>),
}

impl<T> SingleApplication<T>
where
    T: Send + Serialize + DeserializeOwned + 'static,
{
    /// Creates a single-instance application handle.
    ///
    /// # Parameters
    /// - `id`: the single-instance identifier
    ///
    /// # Panics
    /// - Panics if the name conversion fails
    /// - Panics on any other creation failure
    pub fn new(id: &str) -> SingleApplication<T> {
        let name = match id.to_ns_name::<GenericNamespaced>() {
            Ok(name) => name,
            Err(err) => panic!("Convert to namespaced name failed: {}", err),
        };

        #[cfg(not(feature = "tokio"))]
        let listener_result = ListenerOptions::new().name(name).create_sync();

        #[cfg(feature = "tokio")]
        let listener_result = ListenerOptions::new().name(name).create_tokio();

        match listener_result {
            Ok(listener) => {
                SingleApplication::PrimaryInstance(Self::create_single_instance(listener))
            }
            #[cfg(target_os = "windows")]
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                SingleApplication::SecondaryInstance(SecondaryInstance {
                    id: id.to_string(),
                    _phantom: PhantomData,
                })
            }
            #[cfg(not(target_os = "windows"))]
            Err(err) if err.kind() == ErrorKind::AddrInUse => {
                SingleApplication::SecondaryInstance(SecondaryInstance {
                    id: id.to_string(),
                    _phantom: PhantomData,
                })
            }
            Err(err) => panic!("Create local socket failed: {}", err),
        }
    }

    #[cfg(not(feature = "tokio"))]
    fn create_single_instance(
        listener: interprocess::local_socket::Listener,
    ) -> PrimaryInstance<T> {
        let (sender, receiver) = std::sync::mpsc::channel::<T>();
        let _ = std::thread::spawn(move || {
            loop {
                use interprocess::local_socket::traits::ListenerExt as _;
                use std::io::BufReader;

                for conn in listener
                    .incoming()
                    .filter_map(|conn| {
                        conn.map_err(|err| tracing::error!("Incoming connection failed: {err}"))
                            .ok()
                    })
                    .map(BufReader::new)
                {
                    let data = match serde_json::from_reader::<_, T>(conn) {
                        Ok(data) => data,
                        Err(err) => {
                            tracing::error!("Deserialize data failed: {err}");
                            continue;
                        }
                    };
                    if let Err(err) = sender.send(data) {
                        tracing::error!("Send data failed: {err}");
                    }
                }
            }
        });
        PrimaryInstance { receiver }
    }

    #[cfg(feature = "tokio")]
    /// 以一个有界 accept loop owner 管理 listener 和所有已接入消息任务，确保关闭可 join。
    fn create_single_instance(
        listener: interprocess::local_socket::tokio::Listener,
    ) -> PrimaryInstance<T> {
        let (sender, receiver) = tokio::sync::mpsc::channel::<T>(16);
        let (cancellation, mut cancelled) = tokio::sync::watch::channel(false);
        let listener_task = tokio::spawn(async move {
            const MAX_ACTIVE_CONNECTIONS: usize = 16;
            let mut connections = tokio::task::JoinSet::new();
            loop {
                use interprocess::local_socket::traits::tokio::Listener as _;

                tokio::select! {
                    biased;
                    change = cancelled.changed() => {
                        if change.is_err() || *cancelled.borrow() {
                            break;
                        }
                    }
                    joined = connections.join_next(), if !connections.is_empty() => {
                        if let Some(Err(_)) = joined {
                            tracing::warn!("Single-instance message task ended unexpectedly");
                        }
                    }
                    accepted = listener.accept(), if connections.len() < MAX_ACTIVE_CONNECTIONS => {
                        let conn = match accepted {
                            Ok(conn) => conn,
                            Err(error) => {
                                tracing::error!("There was an error with an incoming connection: {error}");
                                break;
                            }
                        };
                        let sender = sender.clone();
                        connections.spawn(async move {
                            use tokio::io::AsyncReadExt;
                            let mut buffer = Vec::new();
                            let mut conn = conn.take((MAX_TOKIO_MESSAGE_BYTES + 1) as u64);

                            if let Err(error) = conn.read_to_end(&mut buffer).await {
                                tracing::error!("Read data failed: {error}");
                                return;
                            };
                            if buffer.len() > MAX_TOKIO_MESSAGE_BYTES {
                                tracing::warn!("Rejected oversized single-instance message");
                                return;
                            }

                            let data = match serde_json::from_slice::<T>(&buffer) {
                                Ok(data) => data,
                                Err(error) => {
                                    tracing::error!("Deserialize data failed: {error}");
                                    return;
                                }
                            };

                            if let Err(error) = sender.send(data).await {
                                tracing::error!("Send data failed: {error}");
                            }
                        });
                    }
                }
            }

            connections.abort_all();
            while connections.join_next().await.is_some() {}
            drop(listener);
        });

        PrimaryInstance {
            receiver,
            shutdown: PrimaryInstanceShutdownHandle {
                cancellation,
                listener_task: Arc::new(tokio::sync::Mutex::new(Some(listener_task))),
            },
        }
    }
}
