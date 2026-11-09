//! 插件宿主请求的有界异步交接；窗口线程与调用方都不等待操作系统线程。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, VecDeque},
    future::{Future, poll_fn},
    pin::Pin,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
};

/// 有界交接失败明确区分过载、关闭和内部同步错误，调用方可据此映射协议错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PluginUiQueueError {
    Full,
    Closed,
    Poisoned,
    InvalidCapacity,
    MissingRoute,
    DuplicateRoute,
}

struct QueueState<M, R> {
    requests: VecDeque<PluginUiRequest<M, R>>,
    completions: Vec<Weak<Completion<R>>>,
    receiver_waker: Option<Waker>,
    senders_alive: bool,
    receiver_open: bool,
}

struct Queue<M, R> {
    capacity: usize,
    state: Mutex<QueueState<M, R>>,
}

struct SenderLifetime<M, R> {
    queue: Weak<Queue<M, R>>,
}

/// 后台持有发送端；最后一个发送端释放后，窗口仍可消费已受理请求。
pub(crate) struct PluginUiSender<M, R> {
    queue: Arc<Queue<M, R>>,
    lifetime: Arc<SenderLifetime<M, R>>,
}

/// 一个窗口驱动独占接收端，避免同一请求被多个窗口竞争消费。
pub(crate) struct PluginUiReceiver<M, R> {
    queue: Arc<Queue<M, R>>,
}

struct CompletionState<R> {
    reply: Option<R>,
    finished: bool,
    caller_alive: bool,
    reply_waker: Option<Waker>,
    cancelled_waker: Option<Waker>,
}

struct Completion<R> {
    state: Mutex<CompletionState<R>>,
    registration: Option<Arc<Mutex<bool>>>,
    retired: AtomicBool,
}

/// 每个请求只允许一次回复；执行副作用前还须由窗口核验实例及目标绑定。
pub(crate) struct PluginUiRequest<M, R> {
    message: Option<M>,
    completion: Option<Arc<Completion<R>>>,
}

/// 调用方等待单次回复；放弃等待会通知窗口，窗口必须停止未提交的操作。
pub(crate) struct PluginUiResponse<R> {
    completion: Arc<Completion<R>>,
}

/// 同一队列的发送端与唯一接收端必须成对创建，避免跨队列误配。
type PluginUiChannel<M, R> = (PluginUiSender<M, R>, PluginUiReceiver<M, R>);

/// 容量同时限制排队和已交给窗口但尚未完成的请求，避免慢弹层绕过上限。
pub(crate) fn plugin_ui_queue<M, R>(
    capacity: usize,
) -> Result<PluginUiChannel<M, R>, PluginUiQueueError> {
    if capacity == 0 {
        return Err(PluginUiQueueError::InvalidCapacity);
    }
    let queue = Arc::new(Queue {
        capacity,
        state: Mutex::new(QueueState {
            requests: VecDeque::new(),
            completions: Vec::new(),
            receiver_waker: None,
            senders_alive: true,
            receiver_open: true,
        }),
    });
    let sender = PluginUiSender {
        lifetime: Arc::new(SenderLifetime {
            queue: Arc::downgrade(&queue),
        }),
        queue: Arc::clone(&queue),
    };
    Ok((sender, PluginUiReceiver { queue }))
}

impl<M, R> Clone for PluginUiSender<M, R> {
    /// 克隆只共享一个发送端生命周期，最后一个克隆释放时才结束接收循环。
    fn clone(&self) -> Self {
        Self {
            queue: Arc::clone(&self.queue),
            lifetime: Arc::clone(&self.lifetime),
        }
    }
}

impl<M, R> PluginUiSender<M, R> {
    /// 入队不等待窗口；满载立即返回，避免后台任务积压无限内存。
    pub(crate) fn try_send(&self, message: M) -> Result<PluginUiResponse<R>, PluginUiQueueError> {
        let (response, receiver_waker) = self.enqueue(message, None)?;
        if let Some(waker) = receiver_waker {
            waker.wake();
        }
        Ok(response)
    }

    /// 入队前绑定实例门禁，并在释放锁后唤醒窗口，避免关闭与重登记之间出现旧请求。
    fn enqueue(
        &self,
        message: M,
        registration: Option<Arc<Mutex<bool>>>,
    ) -> Result<(PluginUiResponse<R>, Option<Waker>), PluginUiQueueError> {
        let mut state = self
            .queue
            .state
            .lock()
            .map_err(|_| PluginUiQueueError::Poisoned)?;
        if !state.receiver_open {
            return Err(PluginUiQueueError::Closed);
        }
        state.completions.retain(|completion| {
            completion
                .upgrade()
                .is_some_and(|completion| !completion.retired.load(Ordering::Acquire))
        });
        if state.completions.len() >= self.queue.capacity
            || state.requests.len() >= self.queue.capacity
        {
            return Err(PluginUiQueueError::Full);
        }
        let completion = Arc::new(Completion {
            state: Mutex::new(CompletionState {
                reply: None,
                finished: false,
                caller_alive: true,
                reply_waker: None,
                cancelled_waker: None,
            }),
            registration,
            retired: AtomicBool::new(false),
        });
        state.completions.push(Arc::downgrade(&completion));
        state.requests.push_back(PluginUiRequest {
            message: Some(message),
            completion: Some(Arc::clone(&completion)),
        });
        Ok((PluginUiResponse { completion }, state.receiver_waker.take()))
    }
}
struct InstanceRoute<M, R> {
    registration: Arc<Mutex<bool>>,
    sender: PluginUiSender<M, R>,
    completions: Vec<Weak<Completion<R>>>,
}

/// 每个完整实例身份只绑定一个窗口队列，关闭实例不会影响同窗口的其他实例。
pub(crate) struct PluginUiRoutes<K: Ord, M, R> {
    entries: Arc<Mutex<BTreeMap<K, InstanceRoute<M, R>>>>,
}

/// 窗口持有登记句柄；关闭原实例时取消其排队和在途调用。
pub(crate) struct PluginUiRouteGuard<K: Ord, M, R> {
    key: K,
    registration: Arc<Mutex<bool>>,
    entries: Arc<Mutex<BTreeMap<K, InstanceRoute<M, R>>>>,
    active: bool,
}

impl<K: Ord, M, R> Clone for PluginUiRoutes<K, M, R> {
    /// 调用方共享同一身份表，实例关闭对所有后台持有者立即生效。
    fn clone(&self) -> Self {
        Self {
            entries: Arc::clone(&self.entries),
        }
    }
}

impl<K: Ord + Clone, M, R> PluginUiRoutes<K, M, R> {
    /// 启动时创建空表；视图必须先登记原窗口才能接收宿主请求。
    pub(crate) fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// 拒绝同一身份重复登记，防止新窗口静默夺走旧实例的请求。
    pub(crate) fn register(
        &self,
        key: K,
        sender: PluginUiSender<M, R>,
    ) -> Result<PluginUiRouteGuard<K, M, R>, PluginUiQueueError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| PluginUiQueueError::Poisoned)?;
        if entries.contains_key(&key) {
            return Err(PluginUiQueueError::DuplicateRoute);
        }
        let registration = Arc::new(Mutex::new(true));
        entries.insert(
            key.clone(),
            InstanceRoute {
                registration: Arc::clone(&registration),
                sender,
                completions: Vec::new(),
            },
        );
        Ok(PluginUiRouteGuard {
            key,
            registration,
            entries: Arc::clone(&self.entries),
            active: true,
        })
    }

    /// 身份查找、入队与取消登记原子衔接；查不到原实例时不得改发当前窗口。
    pub(crate) fn try_send(
        &self,
        key: &K,
        message: M,
    ) -> Result<PluginUiResponse<R>, PluginUiQueueError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| PluginUiQueueError::Poisoned)?;
        let entry = entries
            .get_mut(key)
            .ok_or(PluginUiQueueError::MissingRoute)?;
        let (response, receiver_waker) = entry
            .sender
            .enqueue(message, Some(Arc::clone(&entry.registration)))?;
        entry.completions.retain(|completion| {
            completion
                .upgrade()
                .is_some_and(|completion| !completion.retired.load(Ordering::Acquire))
        });
        entry.completions.push(Arc::downgrade(&response.completion));
        drop(entries);
        if let Some(waker) = receiver_waker {
            waker.wake();
        }
        Ok(response)
    }
}

impl<K: Ord, M, R> PluginUiRouteGuard<K, M, R> {
    /// 先关闭提交门禁再撤销本次登记；唤醒留到锁外，避免旧请求命中新实例或回调重入。
    pub(crate) fn close(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let matches = entries
            .get(&self.key)
            .is_some_and(|entry| Arc::ptr_eq(&entry.registration, &self.registration));
        let route = if matches {
            let mut active = self
                .registration
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *active = false;
            entries.remove(&self.key)
        } else {
            None
        };
        drop(entries);
        if let Some(route) = route {
            for completion in route
                .completions
                .into_iter()
                .filter_map(|completion| completion.upgrade())
            {
                completion.close();
            }
        }
    }
}

impl<K: Ord, M, R> Drop for PluginUiRouteGuard<K, M, R> {
    /// 实例实体意外释放时也执行相同清理，避免旧请求继续作用于新实例。
    fn drop(&mut self) {
        self.close();
    }
}
impl<M, R> Drop for SenderLifetime<M, R> {
    /// 清理阶段即使锁已中毒也只唤醒接收端，不把异常伪装为成功操作。
    fn drop(&mut self) {
        let Some(queue) = self.queue.upgrade() else {
            return;
        };
        let mut state = queue
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.senders_alive = false;
        let waker = state.receiver_waker.take();
        drop(state);
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<M, R> PluginUiReceiver<M, R> {
    /// 使用注册的 Waker 唤醒窗口任务，不轮询、不阻塞 UI 线程或新建运行时。
    pub(crate) async fn next(
        &mut self,
    ) -> Result<Option<PluginUiRequest<M, R>>, PluginUiQueueError> {
        poll_fn(|cx| {
            let mut state = match self.queue.state.lock() {
                Ok(state) => state,
                Err(_) => return Poll::Ready(Err(PluginUiQueueError::Poisoned)),
            };
            if let Some(request) = state.requests.pop_front() {
                return Poll::Ready(Ok(Some(request)));
            }
            if !state.receiver_open || !state.senders_alive {
                return Poll::Ready(Ok(None));
            }
            replace_waker(&mut state.receiver_waker, cx.waker());
            Poll::Pending
        })
        .await
    }
}

impl<M, R> Drop for PluginUiReceiver<M, R> {
    /// 窗口驱动释放时同时唤醒排队和在途请求，避免调用方永远等待已销毁窗口。
    fn drop(&mut self) {
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.receiver_open = false;
        let requests = std::mem::take(&mut state.requests);
        let completions = std::mem::take(&mut state.completions);
        let receiver_waker = state.receiver_waker.take();
        drop(state);
        for completion in completions
            .into_iter()
            .filter_map(|completion| completion.upgrade())
        {
            completion.close();
        }
        drop(requests);
        if let Some(waker) = receiver_waker {
            waker.wake();
        }
    }
}

impl<M, R> PluginUiRequest<M, R> {
    /// 消息只交出一次，回复句柄继续留在请求中以便弹层结束后回传结果。
    pub(crate) fn take_message(&mut self) -> Option<M> {
        self.message.take()
    }

    /// 在回复锁内提交短小的同步副作用，使调用方取消与提交结果有明确先后顺序。
    /// 闭包不得阻塞、等待异步工作或重入此队列；弹层等待必须在提交前结束。
    pub(crate) fn complete_with(mut self, action: impl FnOnce() -> R) -> bool {
        let Some(completion) = self.completion.take() else {
            return false;
        };
        let accepted = completion.complete_with(action);
        completion.retired.store(true, Ordering::Release);
        accepted
    }
}

impl<M, R> Drop for PluginUiRequest<M, R> {
    /// 未回复的请求离开窗口时显式终止，不能让插件把中断误判为成功。
    fn drop(&mut self) {
        if let Some(completion) = self.completion.take() {
            completion.close();
            completion.retired.store(true, Ordering::Release);
        }
    }
}

impl<R> Completion<R> {
    /// 先持有实例门禁再取得回复锁；关闭或取消先取得对应门禁时不执行操作。
    fn complete_with(&self, action: impl FnOnce() -> R) -> bool {
        let route = match self
            .registration
            .as_ref()
            .map(|registration| registration.lock())
        {
            Some(Ok(active)) if *active => Some(active),
            Some(_) => return false,
            None => None,
        };
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.finished || !state.caller_alive {
            return false;
        }
        state.reply = Some(action());
        state.finished = true;
        let reply_waker = state.reply_waker.take();
        let cancelled_waker = state.cancelled_waker.take();
        drop(state);
        drop(route);
        if let Some(waker) = reply_waker {
            waker.wake();
        }
        if let Some(waker) = cancelled_waker {
            waker.wake();
        }
        true
    }
    /// 接收端关闭或请求丢弃时只清理未完成请求，保留此前已提交的回复。
    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.finished {
            return;
        }
        state.finished = true;
        let reply_waker = state.reply_waker.take();
        let cancelled_waker = state.cancelled_waker.take();
        drop(state);
        if let Some(waker) = reply_waker {
            waker.wake();
        }
        if let Some(waker) = cancelled_waker {
            waker.wake();
        }
    }
}

impl<R> PluginUiResponse<R> {
    /// 取消与同步提交共用门禁；已完成结果优先返回，不能把已生效的操作报成取消。
    pub(crate) fn cancel_if_pending(&mut self) -> Result<Option<R>, PluginUiQueueError> {
        let mut state = self
            .completion
            .state
            .lock()
            .map_err(|_| PluginUiQueueError::Poisoned)?;
        if let Some(reply) = state.reply.take() {
            return Ok(Some(reply));
        }
        if state.finished {
            return Err(PluginUiQueueError::Closed);
        }
        state.caller_alive = false;
        let cancelled_waker = state.cancelled_waker.take();
        drop(state);
        if let Some(waker) = cancelled_waker {
            waker.wake();
        }
        Ok(None)
    }
}
impl<R> Future for PluginUiResponse<R> {
    type Output = Result<R, PluginUiQueueError>;

    /// 回复值在锁内只取一次，窗口关闭与内部同步错误均显式传给调用方。
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = match self.completion.state.lock() {
            Ok(state) => state,
            Err(_) => return Poll::Ready(Err(PluginUiQueueError::Poisoned)),
        };
        if let Some(reply) = state.reply.take() {
            return Poll::Ready(Ok(reply));
        }
        if state.finished {
            return Poll::Ready(Err(PluginUiQueueError::Closed));
        }
        replace_waker(&mut state.reply_waker, cx.waker());
        Poll::Pending
    }
}

impl<R> Drop for PluginUiResponse<R> {
    /// 丢弃等待不释放在途请求配额；直到窗口回收请求才能接受额外工作。
    fn drop(&mut self) {
        let mut state = self
            .completion
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.caller_alive = false;
        let waker = state.cancelled_waker.take();
        drop(state);
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// 仅在任务身份变化时替换 Waker，避免持锁期间唤醒和无意义的引用计数操作。
fn replace_waker(slot: &mut Option<Waker>, waker: &Waker) {
    if slot
        .as_ref()
        .is_none_or(|current| !current.will_wake(waker))
    {
        *slot = Some(waker.clone());
    }
}

#[cfg(test)]
#[path = "../../tests/unit/plugin_host_api.rs"]
mod tests;
