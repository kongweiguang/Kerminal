//! 每个插件一个受管 actor，负责代次、心跳和有限崩溃恢复。
//! @author kongweiguang

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    executor::ApplicationTaskExecutor,
    plugin_ports::{
        PluginHostRouter, PluginProcessMessage, PluginProcessPort, PluginResponseObserver,
    },
};
use kerminal_domain::plugins::{PluginId, PluginRuntimeState};
use serde_json::{Value, json};
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    time::{Instant as TokioInstant, sleep_until, timeout_at},
};
use tokio_util::sync::CancellationToken;

use super::{
    process::{FailedProcess, PluginProcess, PluginProcessSpec, StartFailure},
    rpc::{RouterSlot, RpcConnection},
};

/// 全部窗口共享同一个监督器，插件只能从冻结的启动配置中查找。
pub struct ProcessSupervisor {
    slots: BTreeMap<PluginId, Arc<Slot>>,
    router: RouterSlot,
    events: broadcast::Sender<PluginProcessMessage>,
    executor: ApplicationTaskExecutor,
    stopping: AtomicBool,
}

struct Slot {
    spec: PluginProcessSpec,
    state: Mutex<PluginRuntimeState>,
    connection: Mutex<Option<(u64, RpcConnection)>>,
    sender: Mutex<Option<mpsc::Sender<Control>>>,
}

enum Control {
    Activate {
        deadline: Instant,
        retry: bool,
        reply: oneshot::Sender<AppResult<u64>>,
    },
    Shutdown {
        deadline: Instant,
        reply: oneshot::Sender<AppResult<()>>,
    },
    RestartStop {
        deadline: Instant,
        reply: oneshot::Sender<AppResult<()>>,
    },
    HeartbeatFailed(u64),
}

type ActivationWaiter = (Instant, oneshot::Sender<AppResult<u64>>);

enum StartOutcome {
    Ready(PluginProcess),
    Failed(StartFailure),
    Stopped {
        owner: Option<FailedProcess>,
        reply: Option<oneshot::Sender<AppResult<()>>>,
        result: AppResult<()>,
    },
}

enum HandshakeOutcome {
    Finished(AppResult<()>),
    Interrupted(Instant, Option<oneshot::Sender<AppResult<()>>>),
}

impl ProcessSupervisor {
    /// 创建目录和 actor 均延迟到激活时，声明式插件不会无故启动进程。
    pub fn new(
        specs: Vec<PluginProcessSpec>,
        executor: ApplicationTaskExecutor,
    ) -> AppResult<Self> {
        let (events, _) = broadcast::channel(256);
        let mut slots = BTreeMap::new();
        for spec in specs {
            spec.validate()?;
            let id = spec.id.clone();
            let slot = Slot {
                spec,
                state: Mutex::new(PluginRuntimeState::Idle),
                connection: Mutex::new(None),
                sender: Mutex::new(None),
            };
            if slots.insert(id, Arc::new(slot)).is_some() {
                return Err(AppError::conflict("插件进程配置重复"));
            }
        }
        Ok(Self {
            slots,
            router: Arc::new(Mutex::new(None)),
            events,
            executor,
            stopping: AtomicBool::new(false),
        })
    }

    /// 显式重试才清空失败状态；普通调用不会自动反复启动失败插件。
    pub async fn retry(&self, plugin: &PluginId, deadline: Instant) -> AppResult<u64> {
        self.activate_inner(plugin, deadline, true).await
    }

    /// 严格匹配插件身份，未配置进程的静态插件不隐式启动其他目标。
    fn slot(&self, plugin: &PluginId) -> AppResult<Arc<Slot>> {
        self.slots
            .get(plugin)
            .cloned()
            .ok_or_else(|| AppError::not_found("插件进程"))
    }

    /// 首次激活在共享执行器上登记常驻 actor，禁止在关闭后重新创建。
    fn sender(&self, slot: &Arc<Slot>) -> AppResult<mpsc::Sender<Control>> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(AppError::cancelled("插件服务正在关闭"));
        }
        let mut sender = slot
            .sender
            .lock()
            .map_err(|_| AppError::infrastructure("插件监督锁不可用"))?;
        if self.stopping.load(Ordering::Acquire) {
            return Err(AppError::cancelled("插件服务正在关闭"));
        }
        if let Some(existing) = sender.as_ref() {
            return Ok(existing.clone());
        }
        let (created, receiver) = mpsc::channel(64);
        let actor_slot = Arc::clone(slot);
        let actor_sender = created.clone();
        let router = Arc::clone(&self.router);
        let events = self.events.clone();
        let executor = self.executor.clone();
        tokio::spawn(async move {
            let _ = executor
                .run(move |cancellation| async move {
                    actor(
                        actor_slot,
                        receiver,
                        actor_sender,
                        router,
                        events,
                        cancellation,
                    )
                    .await
                })
                .await;
        });
        *sender = Some(created.clone());
        Ok(created)
    }

    /// 队列提交与结果等待都使用同一个截止时间，不因排队延长握手预算。
    async fn activate_inner(
        &self,
        plugin: &PluginId,
        deadline: Instant,
        retry: bool,
    ) -> AppResult<u64> {
        let slot = self.slot(plugin)?;
        let sender = self.sender(&slot)?;
        let (reply, response) = oneshot::channel();
        timeout_at(
            TokioInstant::from_std(deadline),
            sender.send(Control::Activate {
                deadline,
                retry,
                reply,
            }),
        )
        .await
        .map_err(|_| AppError::timeout("插件启动排队"))?
        .map_err(|_| stale())?;
        timeout_at(TokioInstant::from_std(deadline), response)
            .await
            .map_err(|_| AppError::timeout("插件启动"))?
            .map_err(|_| stale())?
    }

    /// 调用时固定代次与连接；重启后旧调用不能转投新的进程。
    fn connection(&self, plugin: &PluginId, generation: u64) -> AppResult<RpcConnection> {
        let slot = self.slot(plugin)?;
        let connection = slot.connection.lock().map_err(|_| stale())?;
        connection
            .as_ref()
            .filter(|(current, _)| *current == generation)
            .map(|(_, rpc)| rpc.clone())
            .ok_or_else(stale)
    }
}

/// 启动期间继续处理关闭与并发激活，取消握手不会丢失已经启动的进程 owner。
async fn start_attempt(
    slot: &Slot,
    generation: u64,
    router: RouterSlot,
    deadline: Instant,
    receiver: &mut mpsc::Receiver<Control>,
    cancellation: &CancellationToken,
    waiters: &mut Vec<ActivationWaiter>,
) -> StartOutcome {
    let spawn = PluginProcess::spawn(&slot.spec, generation, router);
    tokio::pin!(spawn);
    let process = loop {
        tokio::select! {
            biased;
            control = receiver.recv() => match control {
                Some(Control::Activate { deadline, reply, .. }) => waiters.push((deadline, reply)),
                Some(Control::Shutdown { reply, .. }) => {
                    return StartOutcome::Stopped { owner: None, reply: Some(reply), result: Ok(()) };
                }
                Some(Control::RestartStop { reply, .. }) => { let _ = reply.send(Err(AppError::conflict("插件进程正在启动或回收"))); }
                Some(Control::HeartbeatFailed(_)) => {}
                None => return StartOutcome::Stopped { owner: None, reply: None, result: Ok(()) },
            },
            _ = cancellation.cancelled() => return StartOutcome::Stopped { owner: None, reply: None, result: Ok(()) },
            result = &mut spawn => match result {
                Ok(process) => break process,
                Err(failure) => return StartOutcome::Failed(failure),
            },
        }
    };
    let outcome = {
        let handshake = process.handshake(&slot.spec, generation, deadline);
        tokio::pin!(handshake);
        loop {
            tokio::select! {
                biased;
                control = receiver.recv() => match control {
                    Some(Control::Activate { deadline, reply, .. }) => waiters.push((deadline, reply)),
                    Some(Control::Shutdown { deadline, reply }) => break HandshakeOutcome::Interrupted(deadline, Some(reply)),
                    Some(Control::RestartStop { reply, .. }) => { let _ = reply.send(Err(AppError::conflict("插件进程正在启动或回收"))); }
                    Some(Control::HeartbeatFailed(_)) => {}
                    None => break HandshakeOutcome::Interrupted(Instant::now() + Duration::from_secs(2), None),
                },
                _ = cancellation.cancelled() => break HandshakeOutcome::Interrupted(Instant::now() + Duration::from_secs(2), None),
                result = &mut handshake => break HandshakeOutcome::Finished(result),
            }
        }
    };
    match outcome {
        HandshakeOutcome::Finished(Ok(())) => StartOutcome::Ready(process),
        HandshakeOutcome::Finished(Err(error)) => StartOutcome::Failed(StartFailure {
            error,
            owner: Some(FailedProcess::Process(process)),
        }),
        HandshakeOutcome::Interrupted(deadline, reply) => {
            let mut owner = FailedProcess::Process(process);
            let result = owner.force_stop(deadline).await;
            let success = result.is_ok();
            StartOutcome::Stopped {
                owner: (!success).then_some(owner),
                reply,
                result,
            }
        }
    }
}

/// 并发激活共享一个启动结果，但不得超越各自绝对截止时间。
fn answer_waiters(waiters: Vec<ActivationWaiter>, result: AppResult<u64>) {
    for (deadline, reply) in waiters {
        let value = if Instant::now() >= deadline {
            Err(AppError::timeout("插件启动"))
        } else {
            result.clone()
        };
        let _ = reply.send(value);
    }
}

#[async_trait]
impl PluginProcessPort for ProcessSupervisor {
    /// 路由只允许安装一次，避免运行中悄然改变权限策略。
    fn register_host_router(&self, router: Weak<dyn PluginHostRouter>) -> AppResult<()> {
        let mut current = self
            .router
            .lock()
            .map_err(|_| AppError::infrastructure("插件路由锁不可用"))?;
        if current.is_some() {
            return Err(AppError::conflict("插件宿主路由已注册"));
        }
        *current = Some(router);
        Ok(())
    }

    /// 同步快照只报告真实进程状态，不从启用设置推断就绪。
    fn state(&self, plugin: &PluginId) -> AppResult<PluginRuntimeState> {
        let slot = self.slot(plugin)?;
        let state = slot
            .state
            .lock()
            .map_err(|_| AppError::infrastructure("插件状态锁不可用"))?
            .clone();
        Ok(state)
    }

    /// 相同插件的并发激活会在同一 actor 中合并成同一个进程。
    async fn activate(&self, plugin: &PluginId, deadline: Instant) -> AppResult<u64> {
        self.activate_inner(plugin, deadline, false).await
    }

    /// 显式重试清除失败状态，普通激活保持失败，防止意外无限重启。
    async fn retry(&self, plugin: &PluginId, deadline: Instant) -> AppResult<u64> {
        self.activate_inner(plugin, deadline, true).await
    }

    /// 开发重载先由原 actor 回收旧进程，再沿用该 actor 分配递增代次。
    async fn restart(&self, plugin: &PluginId, deadline: Instant) -> AppResult<u64> {
        let slot = self.slot(plugin)?;
        let sender = self.sender(&slot)?;
        let (reply, response) = oneshot::channel();
        timeout_at(
            TokioInstant::from_std(deadline),
            sender.send(Control::RestartStop { deadline, reply }),
        )
        .await
        .map_err(|_| AppError::timeout("插件重启排队"))?
        .map_err(|_| stale())?;
        timeout_at(TokioInstant::from_std(deadline), response)
            .await
            .map_err(|_| AppError::timeout("插件重启回收"))?
            .map_err(|_| stale())??;
        self.activate_inner(plugin, deadline, true).await
    }

    /// 由连接执行请求配额、取消、超时和旧代次隔离。
    async fn call(
        &self,
        plugin: &PluginId,
        generation: u64,
        method: &str,
        params: Value,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> AppResult<Value> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(AppError::cancelled("插件服务正在关闭"));
        }
        self.connection(plugin, generation)?
            .request(method, params, deadline, cancellation)
            .await
    }

    /// 回执观察器由 RPC 读取器按传入帧顺序执行，不能在调用方恢复后补提交。
    async fn call_observed(
        &self,
        plugin: &PluginId,
        generation: u64,
        method: &str,
        params: Value,
        deadline: Instant,
        cancellation: CancellationToken,
        observer: PluginResponseObserver,
    ) -> AppResult<Value> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(AppError::cancelled("插件服务正在关闭"));
        }
        self.connection(plugin, generation)?
            .request_observed(method, params, deadline, cancellation, observer)
            .await
    }

    /// 旧进程的通知不能在重启后落到新的连接。
    async fn notify(
        &self,
        plugin: &PluginId,
        generation: u64,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> AppResult<()> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(AppError::cancelled("插件服务正在关闭"));
        }
        self.connection(plugin, generation)?
            .notify(method, params, deadline)
            .await
    }

    /// 状态事件带原始插件身份与进程代次，订阅落后时可重新读取 state。
    fn subscribe(&self) -> broadcast::Receiver<PluginProcessMessage> {
        self.events.subscribe()
    }

    /// 所有进程先并发收到同一个绝对截止时间，再等待各自的回收回执。
    async fn shutdown_until(&self, deadline: Instant) -> AppResult<()> {
        self.stopping.store(true, Ordering::Release);
        let mut pending = Vec::new();
        for slot in self.slots.values() {
            let sender = slot
                .sender
                .lock()
                .map_err(|_| AppError::infrastructure("插件监督锁不可用"))?
                .clone();
            if let Some(sender) = sender {
                pending.push(tokio::spawn(async move {
                    let (reply, response) = oneshot::channel();
                    timeout_at(
                        TokioInstant::from_std(deadline),
                        sender.send(Control::Shutdown { deadline, reply }),
                    )
                    .await
                    .map_err(|_| AppError::timeout("插件关闭排队"))?
                    .map_err(|_| stale())?;
                    timeout_at(TokioInstant::from_std(deadline), response)
                        .await
                        .map_err(|_| AppError::timeout("插件进程回收"))?
                        .map_err(|_| stale())?
                }));
            }
        }
        let mut failure = None;
        for response in pending {
            let result = timeout_at(TokioInstant::from_std(deadline), response)
                .await
                .map_err(|_| AppError::timeout("插件进程回收"))
                .and_then(|result| result.map_err(|_| stale()))
                .and_then(|result| result);
            if let Err(error) = result {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

/// 单 actor 独占进程所有权，崩溃、重启和关闭不会并发操作同一 child。
async fn actor(
    slot: Arc<Slot>,
    mut receiver: mpsc::Receiver<Control>,
    sender: mpsc::Sender<Control>,
    router: RouterSlot,
    events: broadcast::Sender<PluginProcessMessage>,
    cancellation: CancellationToken,
) -> AppResult<()> {
    let mut process: Option<PluginProcess> = None;
    let mut cleanup: Option<FailedProcess> = None;
    let mut generation = 0_u64;
    let mut crashes = VecDeque::new();
    let mut restart_at: Option<Instant> = None;
    let mut heartbeat = Instant::now() + Duration::from_secs(30);
    let mut failed = false;
    loop {
        if failed && process.is_some() {
            cleanup = process.take().map(FailedProcess::Process);
        }
        if let Some(owner) = cleanup.as_mut() {
            tokio::select! {
                biased;
                control = receiver.recv() => match control {
                    Some(Control::Shutdown { deadline, reply }) => {
                        let result = owner.force_stop(deadline).await;
                        let success = result.is_ok();
                        if success {
                            finish_actor(&slot, &events, generation);
                            let _ = reply.send(result);
                            return Ok(());
                        }
                        let _ = reply.send(result);
                    }
                    Some(Control::Activate { reply, .. }) => { let _ = reply.send(Err(AppError::conflict("插件进程仍在回收"))); }
                    Some(Control::RestartStop { reply, .. }) => { let _ = reply.send(Err(AppError::conflict("插件进程正在启动或回收"))); }
                    Some(Control::HeartbeatFailed(_)) => {}
                    None => {
                        if owner.force_stop(Instant::now() + Duration::from_secs(2)).await.is_ok() { return Ok(()); }
                    }
                },
                _ = tokio::time::sleep(Duration::from_millis(500)) => {
                    if owner.force_stop(Instant::now() + Duration::from_secs(2)).await.is_ok() { cleanup = None; }
                }
            }
            continue;
        }
        if let Some(active) = process.as_mut() {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    let deadline = Instant::now() + Duration::from_secs(2);
                    match active.stop(deadline).await {
                        Ok(()) => { finish_actor(&slot, &events, generation); return Ok(()); },
                        Err(_) => { failed = true; }
                    }
                }
                control = receiver.recv() => {
                    match control {
                        Some(Control::Activate { reply, .. }) => { let _ = reply.send(Ok(generation)); }
                        Some(Control::Shutdown { deadline, reply }) => {
                            publish(&slot, &events, PluginRuntimeState::Stopping, generation);
                            clear_connection(&slot);
                            let result = active.stop(deadline).await;
                            let success = result.is_ok();
                            if success {
                                finish_actor(&slot, &events, generation);
                                let _ = reply.send(result);
                                return Ok(());
                            }
                            let _ = reply.send(result);
                            failed = true;
                        }
                        Some(Control::RestartStop { deadline, reply }) => {
                            publish(&slot, &events, PluginRuntimeState::Stopping, generation);
                            clear_connection(&slot);
                            let result = active.stop(deadline).await;
                            if result.is_ok() {
                                process = None;
                                failed = false;
                                restart_at = None;
                                crashes.clear();
                                publish(&slot, &events, PluginRuntimeState::Idle, generation);
                            } else {
                                failed = true;
                            }
                            let _ = reply.send(result);
                        }
                        Some(Control::HeartbeatFailed(failed_generation)) if failed_generation == generation => {
                            active.rpc.close();
                        }
                        Some(Control::HeartbeatFailed(_)) => {}
                        None => return active.stop(Instant::now() + Duration::from_secs(2)).await,
                    }
                }
                _ = active.child.wait() => {
                    active.rpc.close();
                    clear_connection(&slot);
                    publish(&slot, &events, PluginRuntimeState::Crashed { generation }, generation);
                    if active.force_stop(Instant::now() + Duration::from_secs(3)).await.is_ok() {
                        process = None;
                        restart_at = schedule_restart(&mut crashes);
                        failed = restart_at.is_none();
                    } else { failed = true; }
                    if failed { publish(&slot, &events, PluginRuntimeState::Failed, generation); }
                }
                _ = active.rpc.closed() => {
                    clear_connection(&slot);
                    publish(&slot, &events, PluginRuntimeState::Crashed { generation }, generation);
                    if active.force_stop(Instant::now() + Duration::from_secs(3)).await.is_ok() {
                        process = None;
                        restart_at = schedule_restart(&mut crashes);
                        failed = restart_at.is_none();
                    } else { failed = true; }
                    if failed { publish(&slot, &events, PluginRuntimeState::Failed, generation); }
                }
                _ = sleep_until(TokioInstant::from_std(heartbeat)) => {
                    heartbeat = Instant::now() + Duration::from_secs(30);
                    let rpc = active.rpc.clone();
                    let sender = sender.clone();
                    let ping_generation = generation;
                    tokio::spawn(async move {
                        if rpc.request("$/ping", json!({}), Instant::now() + Duration::from_secs(5), CancellationToken::new()).await.is_err() {
                            let _ = sender.send(Control::HeartbeatFailed(ping_generation)).await;
                        }
                    });
                }
            }
            continue;
        }
        let wake = restart_at.unwrap_or_else(|| Instant::now() + Duration::from_secs(86400));
        tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            _ = sleep_until(TokioInstant::from_std(wake)), if restart_at.is_some() => {
                restart_at = None;
                generation = generation.checked_add(1).ok_or_else(stale)?;
                publish(&slot, &events, PluginRuntimeState::Starting, generation);
                let mut waiters = Vec::new();
                match start_attempt(&slot, generation, Arc::clone(&router), Instant::now() + Duration::from_secs(10), &mut receiver, &cancellation, &mut waiters).await {
                    StartOutcome::Ready(started) => {
                        set_connection(&slot, generation, started.rpc.clone())?;
                        process = Some(started);
                        heartbeat = Instant::now() + Duration::from_secs(30);
                        if let Ok(mut current) = slot.state.lock() { *current = PluginRuntimeState::Ready { generation }; }
                        if let Some(active) = process.as_ref() {
                            let initialized = active.rpc.notify("initialized", json!({}), Instant::now() + Duration::from_secs(5)).await;
                            if initialized.is_err() {
                                active.rpc.close();
                            } else { publish(&slot, &events, PluginRuntimeState::Ready { generation }, generation); }
                            answer_waiters(waiters, initialized.map(|_| generation));
                        }
                    }
                    StartOutcome::Failed(failure) => {
                        answer_waiters(waiters, Err(failure.error));
                        cleanup = failure.owner;
                        restart_at = schedule_restart(&mut crashes);
                        failed = restart_at.is_none();
                        if failed { publish(&slot, &events, PluginRuntimeState::Failed, generation); }
                    }
                    StartOutcome::Stopped { owner, reply, result } => {
                        answer_waiters(waiters, Err(AppError::cancelled("插件服务正在关闭")));
                        let pending = owner.is_some();
                        if let Some(owner) = owner { cleanup = Some(owner); failed = true; }
                        else { finish_actor(&slot, &events, generation); }
                        if let Some(reply) = reply { let _ = reply.send(result); }
                        if !pending { return Ok(()); }
                    }
                }
            }
            control = receiver.recv() => match control {
                Some(Control::Activate { deadline, retry, reply }) => {
                    if failed && !retry {
                        let _ = reply.send(Err(AppError::conflict("插件已失败，请手动重试")));
                        continue;
                    }
                    if restart_at.is_some() && !retry {
                        let _ = reply.send(Err(stale()));
                        continue;
                    }
                    if retry { crashes.clear(); restart_at = None; failed = false; }
                    generation = generation.checked_add(1).ok_or_else(stale)?;
                    publish(&slot, &events, PluginRuntimeState::Starting, generation);
                    let mut waiters = vec![(deadline, reply)];
                    match start_attempt(&slot, generation, Arc::clone(&router), deadline, &mut receiver, &cancellation, &mut waiters).await {
                        StartOutcome::Ready(started) => {
                            set_connection(&slot, generation, started.rpc.clone())?;
                            process = Some(started);
                            heartbeat = Instant::now() + Duration::from_secs(30);
                            if let Ok(mut current) = slot.state.lock() { *current = PluginRuntimeState::Ready { generation }; }
                            let initialized = if let Some(active) = process.as_ref() {
                                active.rpc.notify("initialized", json!({}), deadline).await
                            } else { Err(stale()) };
                            if initialized.is_err() {
                                if let Some(active) = process.as_ref() { active.rpc.close(); }
                            } else {
                                publish(&slot, &events, PluginRuntimeState::Ready { generation }, generation);
                            }
                            answer_waiters(waiters, initialized.map(|_| generation));
                        }
                        StartOutcome::Failed(failure) => {
                            answer_waiters(waiters, Err(failure.error));
                            cleanup = failure.owner;
                            failed = true;
                            publish(&slot, &events, PluginRuntimeState::Failed, generation);
                        }
                        StartOutcome::Stopped { owner, reply, result } => {
                            answer_waiters(waiters, Err(AppError::cancelled("插件服务正在关闭")));
                            let pending = owner.is_some();
                            if let Some(owner) = owner { cleanup = Some(owner); failed = true; }
                            else { finish_actor(&slot, &events, generation); }
                            if let Some(reply) = reply { let _ = reply.send(result); }
                            if !pending { return Ok(()); }
                        }
                    }
                }
                Some(Control::Shutdown { reply, .. }) => {
                    finish_actor(&slot, &events, generation);
                    let _ = reply.send(Ok(()));
                    return Ok(());
                }
                Some(Control::RestartStop { reply, .. }) => { let _ = reply.send(Ok(())); }
                Some(Control::HeartbeatFailed(_)) => {}
                None => return Ok(()),
            }
        }
    }
}

/// 只计算最近 60 秒的崩溃，依次使用 1、4、16 秒退避。
fn schedule_restart(crashes: &mut VecDeque<Instant>) -> Option<Instant> {
    let now = Instant::now();
    while crashes
        .front()
        .is_some_and(|when| now.duration_since(*when) >= Duration::from_secs(60))
    {
        crashes.pop_front();
    }
    crashes.push_back(now);
    [1, 4, 16]
        .get(crashes.len().saturating_sub(1))
        .map(|seconds| now + Duration::from_secs(*seconds))
}

/// 先更新快照再广播，使订阅者补读时不会看到更旧的状态。
fn publish(
    slot: &Slot,
    events: &broadcast::Sender<PluginProcessMessage>,
    state: PluginRuntimeState,
    generation: u64,
) {
    let label = match &state {
        PluginRuntimeState::Idle => "idle",
        PluginRuntimeState::Starting => "starting",
        PluginRuntimeState::Ready { .. } => "ready",
        PluginRuntimeState::Crashed { .. } => "crashed",
        PluginRuntimeState::Failed => "failed",
        PluginRuntimeState::Stopping => "stopping",
    };
    if let Ok(mut current) = slot.state.lock() {
        *current = state;
    }
    let _ = events.send(PluginProcessMessage {
        plugin: slot.spec.id.clone(),
        generation,
        method: "$/process-state".into(),
        params: json!({"state":label}),
    });
}

/// 只有握手成功的连接才能对外提供，代次与句柄在同一个锁下提交。
fn set_connection(slot: &Slot, generation: u64, rpc: RpcConnection) -> AppResult<()> {
    *slot
        .connection
        .lock()
        .map_err(|_| AppError::infrastructure("插件连接锁不可用"))? = Some((generation, rpc));
    Ok(())
}

/// 先移除旧连接再发布崩溃，防止回调命中新代次。
fn clear_connection(slot: &Slot) {
    if let Ok(mut current) = slot.connection.lock() {
        *current = None;
    }
}

/// 清理完成后撤销控制通道，使重复关闭能够幂等返回。
fn finish_actor(slot: &Slot, events: &broadcast::Sender<PluginProcessMessage>, generation: u64) {
    clear_connection(slot);
    publish(slot, events, PluginRuntimeState::Idle, generation);
    if let Ok(mut sender) = slot.sender.lock() {
        *sender = None;
    }
}

/// 不把崩溃解释成任何请求的成功或自动重试。
fn stale() -> AppError {
    AppError::conflict("插件进程代次已失效")
}
