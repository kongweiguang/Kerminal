//! ApplicationTaskExecutor 的取消、关闭和 runtime 调度契约。
//!
//! @author kongweiguang

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use kerminal_application::{AppError, executor::ApplicationTaskExecutor};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// 使用 Tokio timer 验证被调度 future 在注入的应用 runtime 上执行。
#[tokio::test]
async fn run_polls_task_on_application_runtime() {
    let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
    let result = executor
        .run(|_| async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            Ok::<_, AppError>(42_u8)
        })
        .await;

    assert_eq!(result.expect("scheduled task should complete"), 42);
    executor
        .shutdown()
        .await
        .expect("executor should close cleanly");
}

/// 已受监督任务在提交副作用后 panic 且丢失回执时，executor 必须返回 OutcomeUnknown，避免安全重放。
///
/// @author kongweiguang
#[tokio::test]
async fn accepted_task_losing_result_after_submission_is_outcome_unknown() {
    let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
    let submitted = Arc::new(AtomicBool::new(false));
    let submissions = Arc::new(AtomicUsize::new(0));
    let submitted_by_task = submitted.clone();
    let submissions_by_task = submissions.clone();

    let result = executor
        .run::<(), _, _>(move |_| async move {
            submitted_by_task.store(true, Ordering::SeqCst);
            submissions_by_task.fetch_add(1, Ordering::SeqCst);
            panic!("simulated panic after submission");
        })
        .await;

    assert!(submitted.load(Ordering::SeqCst));
    assert_eq!(submissions.load(Ordering::SeqCst), 1);
    assert_eq!(
        result
            .expect_err("lost task receipt must not be reported as a certain failure")
            .public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );
    executor
        .shutdown()
        .await
        .expect("supervisor should join the panicked task");
}

/// 丢弃 UI 等待 future 时发出 token，应用退出仍 join 对应运行态 task。
#[tokio::test]
async fn dropping_waiter_cancels_and_shutdown_joins_task() {
    let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
    let (started_sender, started_receiver) = oneshot::channel();
    let task_finished = Arc::new(AtomicBool::new(false));
    let task_finished_from_worker = task_finished.clone();
    let executor_for_waiter = executor.clone();
    let waiter = tokio::spawn(async move {
        executor_for_waiter
            .run(move |cancellation| async move {
                let _ = started_sender.send(());
                cancellation.cancelled().await;
                task_finished_from_worker.store(true, Ordering::SeqCst);
                Err::<(), _>(AppError::cancelled("测试任务收到取消"))
            })
            .await
    });

    started_receiver
        .await
        .expect("tracked worker should have started");
    waiter.abort();
    let _ = waiter.await;

    tokio::time::timeout(Duration::from_secs(1), executor.shutdown())
        .await
        .expect("shutdown should join a cooperative task")
        .expect("shutdown should succeed");
    assert!(task_finished.load(Ordering::SeqCst));
}

/// 调用方重复请求取消时仍等待用例给出提交回执，不把取消意图伪装成写入结果。
#[tokio::test]
async fn run_cancellable_waits_for_receipt_after_repeated_cancel_requests() {
    let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
    let caller_cancellation = CancellationToken::new();
    let cancellation_for_waiter = caller_cancellation.clone();
    let (started_sender, started_receiver) = oneshot::channel();
    let (cancelled_sender, cancelled_receiver) = oneshot::channel();
    let (receipt_sender, receipt_receiver) = oneshot::channel();
    let executor_for_waiter = executor.clone();
    let waiter = tokio::spawn(async move {
        executor_for_waiter
            .run_cancellable(
                cancellation_for_waiter,
                move |operation_cancellation| async move {
                    let _ = started_sender.send(());
                    operation_cancellation.cancelled().await;
                    let _ = cancelled_sender.send(());
                    receipt_receiver
                        .await
                        .map_err(|_| AppError::infrastructure("测试收据通道提前关闭"))
                },
            )
            .await
    });

    started_receiver
        .await
        .expect("tracked write should have started");
    caller_cancellation.cancel();
    caller_cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(1), cancelled_receiver)
        .await
        .expect("operation should observe a single cooperative cancellation")
        .expect("operation should acknowledge cancellation");
    assert!(
        !waiter.is_finished(),
        "the caller must keep waiting for the actual operation receipt"
    );

    receipt_sender
        .send(23_u8)
        .expect("operation should still own its result sender");
    assert_eq!(
        waiter
            .await
            .expect("waiter should receive the actual result")
            .expect("a committed operation may succeed after cancellation was requested"),
        23
    );
    executor
        .shutdown()
        .await
        .expect("executor should close cleanly");
}

/// 提交前协作取消由业务用例回报 Cancelled，executor 不自行合成终态。
#[tokio::test]
async fn run_cancellable_returns_the_operation_cancel_receipt() {
    let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
    let caller_cancellation = CancellationToken::new();
    let cancellation_for_waiter = caller_cancellation.clone();
    let (started_sender, started_receiver) = oneshot::channel();
    let (cancelled_sender, cancelled_receiver) = oneshot::channel();
    let executor_for_waiter = executor.clone();
    let waiter = tokio::spawn(async move {
        executor_for_waiter
            .run_cancellable(
                cancellation_for_waiter,
                move |operation_cancellation| async move {
                    let _ = started_sender.send(());
                    operation_cancellation.cancelled().await;
                    let _ = cancelled_sender.send(());
                    Err::<(), _>(AppError::cancelled("写入尚未提交，已安全取消"))
                },
            )
            .await
    });

    started_receiver
        .await
        .expect("tracked write should have started");
    caller_cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(1), cancelled_receiver)
        .await
        .expect("operation should observe the cancellation request")
        .expect("operation should acknowledge cancellation");
    assert_eq!(
        waiter
            .await
            .expect("waiter should finish after the operation")
            .expect_err("the operation reports its safe cancellation receipt")
            .public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    executor
        .shutdown()
        .await
        .expect("executor should close cleanly");
}

/// 预先发出的取消请求必须在用例构造和首次轮询前传播到业务 token。
#[tokio::test]
async fn run_cancellable_propagates_pre_cancel_before_building_operation() {
    let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
    let caller_cancellation = CancellationToken::new();
    caller_cancellation.cancel();
    let (observed_sender, observed_receiver) = oneshot::channel();

    let result = executor
        .run_cancellable(caller_cancellation, move |operation_cancellation| {
            let observed_before_poll = operation_cancellation.is_cancelled();
            let _ = observed_sender.send(observed_before_poll);
            async move {
                if operation_cancellation.is_cancelled() {
                    Err(AppError::cancelled("用例启动前已收到取消请求"))
                } else {
                    Ok(())
                }
            }
        })
        .await;

    assert!(
        observed_receiver
            .await
            .expect("builder should inspect the operation token"),
        "the operation must receive cancellation before construction"
    );
    assert_eq!(
        result
            .expect_err("the operation should report its cancellation receipt")
            .public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    executor
        .shutdown()
        .await
        .expect("executor should close cleanly");
}

/// 应用关闭仍通过同一个业务 token 协作取消并 join cancellable operation。
#[tokio::test]
async fn run_cancellable_is_joined_when_executor_shuts_down() {
    let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
    let (started_sender, started_receiver) = oneshot::channel();
    let (cancelled_sender, cancelled_receiver) = oneshot::channel();
    let executor_for_waiter = executor.clone();
    let waiter = tokio::spawn(async move {
        executor_for_waiter
            .run_cancellable(
                CancellationToken::new(),
                move |operation_cancellation| async move {
                    let _ = started_sender.send(());
                    operation_cancellation.cancelled().await;
                    let _ = cancelled_sender.send(());
                    Err::<(), _>(AppError::cancelled("应用关闭前完成清理"))
                },
            )
            .await
    });

    started_receiver
        .await
        .expect("tracked write should have started");
    executor
        .shutdown()
        .await
        .expect("shutdown should wait for cooperative cancellation receipt");
    cancelled_receiver
        .await
        .expect("operation should observe supervisor cancellation");
    assert_eq!(
        waiter
            .await
            .expect("waiter should finish before shutdown returns")
            .expect_err("shutdown cancellation is reported by the operation")
            .public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
}

/// 关闭 intake 后拒绝后续提交为 Cancelled，因为任务从未进入受监督队列。
///
/// @author kongweiguang
#[tokio::test]
async fn shutdown_rejects_new_tasks() {
    let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
    executor
        .shutdown()
        .await
        .expect("executor should close cleanly");
    let task_started = Arc::new(AtomicBool::new(false));
    let task_started_by_closure = task_started.clone();

    let result = executor
        .run(move |_| async move {
            task_started_by_closure.store(true, Ordering::SeqCst);
            Ok::<_, AppError>(())
        })
        .await;

    assert_eq!(
        result
            .expect_err("unaccepted task must be rejected before execution")
            .public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    assert!(!task_started.load(Ordering::SeqCst));
}

/// 共享退出预算耗尽时及时返回，后续仍可在同一 supervisor 上完成实际 join。
#[tokio::test]
async fn shutdown_until_obeys_deadline_without_detaching_task() {
    let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
    let (started_sender, started_receiver) = oneshot::channel();
    let executor_for_waiter = executor.clone();
    let waiter = tokio::spawn(async move {
        executor_for_waiter
            .run(move |_| async move {
                let _ = started_sender.send(());
                tokio::time::sleep(Duration::from_millis(40)).await;
                Ok::<_, AppError>(())
            })
            .await
    });
    started_receiver
        .await
        .expect("tracked worker should have started");

    let deadline = tokio::time::Instant::now() + Duration::from_millis(1);
    let first_shutdown = executor.shutdown_until(deadline).await;
    assert!(
        first_shutdown.is_err(),
        "non-cooperative work must respect deadline"
    );

    executor
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .expect("the tracked task should remain joinable after the first deadline");
    waiter
        .await
        .expect("waiter task should complete")
        .expect("application task should return normally");
}

/// watch 只证明 intake 与受管任务；关闭超时仍保留原任务计数，重试不能重新打开入口。
///
/// @author kongweiguang
#[tokio::test]
async fn lifecycle_snapshot_keeps_closing_owner_until_the_real_receipt() {
    let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
    let state = executor.subscribe_state();
    assert!(state.borrow().accepting);
    assert_eq!(state.borrow().active_tasks, 0);

    let (started_sender, started_receiver) = oneshot::channel();
    let (receipt_sender, receipt_receiver) = oneshot::channel();
    let executor_for_waiter = executor.clone();
    let waiter = tokio::spawn(async move {
        executor_for_waiter
            .run(move |_| async move {
                let _ = started_sender.send(());
                receipt_receiver
                    .await
                    .map_err(|_| AppError::infrastructure("测试回执通道提前关闭"))
            })
            .await
    });
    started_receiver
        .await
        .expect("task admitted before shutdown");
    assert!(state.borrow().accepting);
    assert_eq!(state.borrow().active_tasks, 1);

    executor.begin_shutdown().expect("close intake");
    assert!(!state.borrow().accepting);
    assert_eq!(state.borrow().active_tasks, 1);
    assert!(
        executor
            .shutdown_until(tokio::time::Instant::now())
            .await
            .is_err()
    );
    assert_eq!(
        state.borrow().active_tasks,
        1,
        "timeout must retain the owner"
    );
    assert!(
        executor
            .run(|_| async { Ok::<_, AppError>(()) })
            .await
            .is_err()
    );
    assert_eq!(
        state.borrow().active_tasks,
        1,
        "rejected work is never counted"
    );

    receipt_sender.send(7_u8).expect("release the real task");
    assert_eq!(waiter.await.expect("join waiter").expect("real receipt"), 7);
    executor
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .expect("retry joins the same task tracker");
    assert!(!state.borrow().accepting);
    assert_eq!(state.borrow().active_tasks, 0);
}
