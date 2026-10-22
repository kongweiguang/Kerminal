//! Listener join 的取消安全回归；用受控任务验证 owner 不因等待 future 被丢弃而失联。
//!
//! @author kongweiguang

use std::{
    future::{Future, poll_fn},
    task::Poll,
    time::Duration,
};

use tokio::{
    sync::oneshot,
    time::{Instant, timeout_at},
};
use tokio_util::task::TaskTracker;

use super::join_server_task;

/// 单次 poll 后真正丢弃等待 future；断言前释放 gate 并等待 tracker，旧实现失败也不泄漏任务。
///
/// @author kongweiguang
#[tokio::test]
async fn dropping_listener_join_future_preserves_owner_for_retry() {
    let tasks = TaskTracker::new();
    let (release, blocked) = oneshot::channel::<()>();
    let mut owner = Some(tasks.spawn(async move {
        let _ = blocked.await;
        Ok(())
    }));
    let pending = {
        let mut stopping = Box::pin(join_server_task(
            &mut owner,
            Instant::now() + Duration::from_secs(2),
        ));
        poll_fn(|cx| Poll::Ready(stopping.as_mut().poll(cx)))
            .await
            .is_pending()
    };
    let owner_preserved = owner.is_some();
    let released = release.send(()).is_ok();
    let retried = join_server_task(&mut owner, Instant::now() + Duration::from_secs(2)).await;
    tasks.close();
    let drained = timeout_at(Instant::now() + Duration::from_secs(2), tasks.wait()).await;

    assert!(
        drained.is_ok(),
        "the controlled task must finish before assertions"
    );
    assert!(
        pending,
        "the first join poll must wait on the controlled gate"
    );
    assert!(
        owner_preserved,
        "dropping the wait must retain the listener owner"
    );
    assert!(
        released,
        "the controlled task must remain alive until released"
    );
    assert!(
        retried.is_ok(),
        "the retained owner must support a fresh join"
    );
    assert!(owner.is_none(), "only the completed join clears its owner");
}

/// 过期 deadline 不消费句柄；同一受控任务放行后必须能用新 deadline 回收，而非留下 detached worker。
///
/// @author kongweiguang
#[tokio::test]
async fn listener_join_timeout_preserves_owner_for_retry() {
    let tasks = TaskTracker::new();
    let (release, blocked) = oneshot::channel::<()>();
    let mut owner = Some(tasks.spawn(async move {
        let _ = blocked.await;
        Ok(())
    }));
    let timed_out = join_server_task(&mut owner, Instant::now() - Duration::from_millis(1)).await;
    let owner_preserved = owner.is_some();
    let released = release.send(()).is_ok();
    let retried = join_server_task(&mut owner, Instant::now() + Duration::from_secs(2)).await;
    tasks.close();
    let drained = timeout_at(Instant::now() + Duration::from_secs(2), tasks.wait()).await;

    assert!(
        drained.is_ok(),
        "the controlled task must finish before assertions"
    );
    assert!(
        timed_out.is_err(),
        "the blocked task must exceed the expired deadline"
    );
    assert!(owner_preserved, "timeout must retain the listener owner");
    assert!(
        released,
        "the controlled task must remain alive until released"
    );
    assert!(
        retried.is_ok(),
        "the retained owner must support a fresh join"
    );
    assert!(owner.is_none(), "only the completed join clears its owner");
}
