//! 将 GPUI 等外部执行器发起的用例 future 投递到应用唯一 Tokio runtime。
//!
//! @author kongweiguang

use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    runtime::Handle,
    sync::{oneshot, watch},
    time::{Instant, timeout_at},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{AppError, AppResult};

/// 可克隆的异步执行入口；提交任务均进入共享 tracker 并支持协作取消。
#[derive(Clone)]
pub struct ApplicationTaskExecutor {
    runtime: Handle,
    supervisor: Arc<TaskSupervisor>,
}

/// 对外报告任务 intake 与受管任务数量；不将 executor 空闲误报为宿主全部清理成功。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApplicationTaskSnapshot {
    /// false 表示应用已封口，不会再接受新的后台用例。
    pub accepting: bool,
    /// 当前由 supervisor 持有且尚未返回的后台用例数。
    pub active_tasks: usize,
}

/// 共享 supervisor 在一个短锁区间内关闭 intake 或登记任务，消除退出与提交竞态。
struct TaskSupervisor {
    lifecycle: Arc<Mutex<ApplicationTaskSnapshot>>,
    updates: watch::Sender<ApplicationTaskSnapshot>,
    cancellation: CancellationToken,
    tasks: TaskTracker,
}

/// 等待方被丢弃时取消对应业务 token；Tokio task 仍由 tracker 负责收尾与 join。
struct CancelOnDrop {
    cancellation: CancellationToken,
    armed: bool,
}

impl ApplicationTaskExecutor {
    /// 只接受 composition root 持有的 runtime handle，避免每个界面或服务创建线程池。
    /// 同时发布零活动任务的初始快照，供宿主从同一个 executor 读取服务状态。
    ///
    /// @author kongweiguang
    pub fn new(runtime: Handle) -> Self {
        let initial_snapshot = ApplicationTaskSnapshot {
            accepting: true,
            active_tasks: 0,
        };
        let (updates, _) = watch::channel(initial_snapshot);
        Self {
            runtime,
            supervisor: Arc::new(TaskSupervisor {
                lifecycle: Arc::new(Mutex::new(initial_snapshot)),
                updates,
                cancellation: CancellationToken::new(),
                tasks: TaskTracker::new(),
            }),
        }
    }

    /// 订阅真实 intake/任务计数变化，供宿主界面在关闭刚开始时撤下活工作台。
    ///
    /// @author kongweiguang
    pub fn subscribe_state(&self) -> watch::Receiver<ApplicationTaskSnapshot> {
        self.supervisor.updates.subscribe()
    }

    /// 把任务在应用 runtime 执行并登记；等待方取消只触发 token，不会 detach 或粗暴 abort。
    pub async fn run<T, Build, Fut>(&self, build: Build) -> AppResult<T>
    where
        T: Send + 'static,
        Build: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = AppResult<T>> + Send + 'static,
    {
        self.run_inner(None, build).await
    }

    /// 允许调用方请求业务取消，但保留任务 future 直到用例返回真实完成或未知结果收据。
    pub async fn run_cancellable<T, Build, Fut>(
        &self,
        caller_cancellation: CancellationToken,
        build: Build,
    ) -> AppResult<T>
    where
        T: Send + 'static,
        Build: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = AppResult<T>> + Send + 'static,
    {
        self.run_inner(Some(caller_cancellation), build).await
    }

    /// 共享受监督执行路径；额外取消源只通知业务 token，不会丢弃正在提交的写入 future。
    ///
    /// @author kongweiguang
    async fn run_inner<T, Build, Fut>(
        &self,
        caller_cancellation: Option<CancellationToken>,
        build: Build,
    ) -> AppResult<T>
    where
        T: Send + 'static,
        Build: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = AppResult<T>> + Send + 'static,
    {
        let cancellation = self.supervisor.cancellation.child_token();
        let task_cancellation = cancellation.clone();
        let (result_sender, result_receiver) = oneshot::channel();
        let task = async move {
            if caller_cancellation
                .as_ref()
                .is_some_and(|cancellation| cancellation.is_cancelled())
            {
                task_cancellation.cancel();
            }
            let operation = build(task_cancellation.clone());
            let result = if let Some(caller_cancellation) = caller_cancellation {
                tokio::pin!(operation);
                tokio::select! {
                    biased;
                    _ = caller_cancellation.cancelled() => {
                        task_cancellation.cancel();
                        operation.await
                    },
                    result = &mut operation => result,
                }
            } else {
                operation.await
            };
            let _ = result_sender.send(result);
        };

        self.supervisor.spawn(&self.runtime, task)?;
        let mut cancel_on_drop = CancelOnDrop::new(cancellation);
        let result = result_receiver
            .await
            .map_err(|_| AppError::outcome_unknown("后台用例任务终止且未返回结果"))?;
        cancel_on_drop.disarm();
        result
    }

    /// 关闭新任务入口并协作取消现有任务；资源清理由带统一截止时间的等待阶段完成。
    pub fn begin_shutdown(&self) -> AppResult<()> {
        let close_result = self.supervisor.close_intake();
        self.supervisor.cancellation.cancel();
        self.supervisor.tasks.close();
        close_result
    }

    /// 在应用级统一截止时间前等待已登记任务收尾，超时后保留 tracker 以供最终 runtime 回收。
    pub async fn shutdown_until(&self, deadline: Instant) -> AppResult<()> {
        let close_result = self.begin_shutdown();
        let wait_result = timeout_at(deadline, self.supervisor.tasks.wait())
            .await
            .map_err(|_| AppError::timeout("后台任务收尾"));
        close_result?;
        wait_result
    }

    /// 兼容独立调用方的默认关闭入口；桌面组合根应传入共享的绝对截止时间。
    pub async fn shutdown(&self) -> AppResult<()> {
        self.shutdown_until(Instant::now() + Duration::from_secs(5))
            .await
    }
}

impl TaskSupervisor {
    /// 在同一锁区间验证并向 tracker 登记，再释放锁后调度。
    /// 已停止的 runtime 可能同步丢弃 future，不能让 RAII 计数回写重入仍持有的生命周期锁。
    ///
    /// @author kongweiguang
    fn spawn<F>(&self, runtime: &Handle, task: F) -> AppResult<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| AppError::infrastructure("后台任务 supervisor 锁不可用"))?;
        if !lifecycle.accepting {
            return Err(AppError::cancelled("应用正在关闭，拒绝新后台任务"));
        }
        lifecycle.active_tasks = lifecycle
            .active_tasks
            .checked_add(1)
            .ok_or_else(|| AppError::infrastructure("后台任务计数耗尽"))?;
        self.updates.send_replace(*lifecycle);

        let completion = TaskCompletion {
            lifecycle: Arc::clone(&self.lifecycle),
            updates: self.updates.clone(),
        };
        let tracked = self.tasks.track_future(async move {
            let _completion = completion;
            task.await;
        });
        drop(lifecycle);
        // JoinHandle 不承载所有权：tracker 已在封口锁内登记，负责保留关闭时的等待能力。
        drop(runtime.spawn(tracked));
        Ok(())
    }

    /// 同步关闭 intake；锁中毒时恢复两个可复制状态字段，确保退出仍可封口。
    ///
    /// @author kongweiguang
    fn close_intake(&self) -> AppResult<()> {
        let mut lifecycle = self
            .lifecycle
            .lock()
            // 状态仅含两个可复制计数/标记；即使先前持锁线程 panic，也可安全完成封口。
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lifecycle.accepting = false;
        self.updates.send_replace(*lifecycle);
        Ok(())
    }
}

/// Future 正常返回或 panic unwind 时都发布准确的受管任务数量，供关闭界面刷新。
struct TaskCompletion {
    lifecycle: Arc<Mutex<ApplicationTaskSnapshot>>,
    updates: watch::Sender<ApplicationTaskSnapshot>,
}

impl Drop for TaskCompletion {
    /// RAII 统计覆盖 panic 与取消路径，避免“已关闭”状态漏掉仍由 tracker 管理的任务。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lifecycle.active_tasks = lifecycle.active_tasks.saturating_sub(1);
        self.updates.send_replace(*lifecycle);
    }
}

impl CancelOnDrop {
    /// 新建尚未完成的等待守卫，确保调用侧 future 被丢弃时通知具体任务。
    fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            armed: true,
        }
    }

    /// 已收到任务结果后解除守卫，避免把正常完成误报为取消。
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    /// GPUI 切换视图或退出时只发协作取消，后台 task 仍保持受 supervisor 管理。
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
        }
    }
}
