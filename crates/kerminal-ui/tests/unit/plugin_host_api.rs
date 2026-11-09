//! 有界插件 UI 请求通道的背压、唤醒和生命周期回归。
//!
//! @author kongweiguang

use super::*;
use std::{
    sync::{Arc, Barrier, atomic::AtomicUsize},
    task::Wake,
};

struct WakeCounter(AtomicUsize);

impl Wake for WakeCounter {
    /// 记录真实唤醒次数，验证后台发送不会依赖窗口轮询。
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    /// 借用唤醒与消费唤醒使用相同计数，以覆盖执行器的两种调用方式。
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// 测试中只对已确定可完成的 future 使用同步单次轮询，避免引入额外运行时。
fn ready<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("future should already be ready"),
    }
}

/// 在途弹层也占用容量，避免从队列取走后绕过背压上限。
#[test]
fn queue_bounds_queued_and_in_flight_requests() {
    let (sender, mut receiver) = plugin_ui_queue::<&str, u32>(1).unwrap();
    let reply = sender.try_send("first").unwrap();
    let mut request = ready(receiver.next()).unwrap().unwrap();
    assert_eq!(request.take_message(), Some("first"));
    assert_eq!(request.take_message(), None);
    assert!(matches!(
        sender.try_send("overflow"),
        Err(PluginUiQueueError::Full)
    ));
    assert!(request.respond(42));
    assert_eq!(ready(reply), Ok(42));
    assert!(sender.try_send("after completion").is_ok());
}

/// 发送与接收的唤醒跨线程交接，窗口无需定时扫描队列。
#[test]
fn sender_wakes_a_waiting_receiver_from_another_thread() {
    let (sender, mut receiver) = plugin_ui_queue::<u32, u32>(1).unwrap();
    let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&counter));
    let mut context = Context::from_waker(&waker);
    let mut next = Box::pin(receiver.next());
    assert!(matches!(next.as_mut().poll(&mut context), Poll::Pending));
    let reply = std::thread::spawn(move || sender.try_send(7).unwrap())
        .join()
        .unwrap();
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    let mut request = match next.as_mut().poll(&mut context) {
        Poll::Ready(Ok(Some(request))) => request,
        _ => panic!("queued request should wake the receiver"),
    };
    assert_eq!(request.take_message(), Some(7));
    assert!(request.respond(8));
    assert_eq!(ready(reply), Ok(8));
}

/// 调用方取消能唤醒弹层；在窗口回收请求之前不能释放其配额。
#[test]
fn abandoning_a_reply_wakes_the_request_and_keeps_its_slot() {
    let (sender, mut receiver) = plugin_ui_queue::<u32, u32>(1).unwrap();
    let reply = sender.try_send(1).unwrap();
    let request = ready(receiver.next()).unwrap().unwrap();
    let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&counter));
    let mut context = Context::from_waker(&waker);
    let mut cancelled = Box::pin(request.cancelled());
    assert!(matches!(
        cancelled.as_mut().poll(&mut context),
        Poll::Pending
    ));
    drop(reply);
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    assert!(matches!(
        cancelled.as_mut().poll(&mut context),
        Poll::Ready(())
    ));
    drop(cancelled);
    assert!(request.is_cancelled());
    assert!(matches!(sender.try_send(2), Err(PluginUiQueueError::Full)));
    drop(request);
    assert!(sender.try_send(3).is_ok());
}

/// 窗口销毁会关闭排队和在途回复，同时使原回调失效。
#[test]
fn closing_receiver_cancels_queued_and_in_flight_requests() {
    let (sender, mut receiver) = plugin_ui_queue::<u32, u32>(2).unwrap();
    let in_flight_reply = sender.try_send(1).unwrap();
    let in_flight = ready(receiver.next()).unwrap().unwrap();
    let queued_reply = sender.try_send(2).unwrap();
    drop(receiver);
    assert_eq!(ready(in_flight_reply), Err(PluginUiQueueError::Closed));
    assert_eq!(ready(queued_reply), Err(PluginUiQueueError::Closed));
    assert!(in_flight.is_cancelled());
    assert!(!in_flight.respond(9));
    assert!(matches!(
        sender.try_send(3),
        Err(PluginUiQueueError::Closed)
    ));
}

/// 已返回的结果不会被随后发生的窗口关闭改写成失败。
#[test]
fn closing_receiver_preserves_a_completed_reply() {
    let (sender, mut receiver) = plugin_ui_queue::<(), u32>(1).unwrap();
    let reply = sender.try_send(()).unwrap();
    let request = ready(receiver.next()).unwrap().unwrap();
    assert!(request.respond(3));
    drop(receiver);
    assert_eq!(ready(reply), Ok(3));
}

/// 最后一个发送端关闭后仍消费已排队的请求，之后接收循环自然结束。
#[test]
fn last_sender_drop_drains_the_existing_queue() {
    let (sender, mut receiver) = plugin_ui_queue::<u32, u32>(1).unwrap();
    let sender_clone = sender.clone();
    let reply = sender.try_send(1).unwrap();
    drop(sender);
    drop(sender_clone);
    let request = ready(receiver.next()).unwrap().unwrap();
    assert!(request.respond(2));
    assert_eq!(ready(reply), Ok(2));
    assert!(matches!(ready(receiver.next()), Ok(None)));
}

/// 并发生产者共用同一容量门禁，不能因竞争而超额接收请求。
#[test]
fn concurrent_producers_cannot_exceed_capacity() {
    let (sender, mut receiver) = plugin_ui_queue::<usize, usize>(3).unwrap();
    let barrier = Arc::new(Barrier::new(12));
    let mut threads = Vec::new();
    for index in 0..12 {
        let sender = sender.clone();
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            sender.try_send(index)
        }));
    }
    let mut replies = Vec::new();
    for thread in threads {
        match thread.join().unwrap() {
            Ok(reply) => replies.push(reply),
            Err(error) => assert_eq!(error, PluginUiQueueError::Full),
        }
    }
    assert_eq!(replies.len(), 3);
    for _ in 0..3 {
        let mut request = ready(receiver.next()).unwrap().unwrap();
        let message = request.take_message().unwrap();
        assert!(request.respond(message));
    }
    assert_eq!(
        replies.into_iter().map(ready).filter(Result::is_ok).count(),
        3
    );
}

/// 无容量的队列不应进入运行态，避免所有请求都表现为偶发过载。
#[test]
fn zero_capacity_is_rejected() {
    assert!(matches!(
        plugin_ui_queue::<(), ()>(0),
        Err(PluginUiQueueError::InvalidCapacity)
    ));
}

/// 请求在窗口交互中被丢弃时立即唤醒等待者，不能留下永不结束的调用。
#[test]
fn dropping_an_unanswered_request_wakes_the_reply() {
    let (sender, mut receiver) = plugin_ui_queue::<(), ()>(1).unwrap();
    let reply = sender.try_send(()).unwrap();
    let request = ready(receiver.next()).unwrap().unwrap();
    let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&counter));
    let mut context = Context::from_waker(&waker);
    let mut reply = Box::pin(reply);
    assert!(matches!(reply.as_mut().poll(&mut context), Poll::Pending));
    drop(request);
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    assert!(matches!(
        reply.as_mut().poll(&mut context),
        Poll::Ready(Err(PluginUiQueueError::Closed))
    ));
}

/// 无新消息时最后一个发送端释放也必须唤醒窗口接收循环。
#[test]
fn dropping_last_sender_wakes_an_idle_receiver() {
    let (sender, mut receiver) = plugin_ui_queue::<(), ()>(1).unwrap();
    let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&counter));
    let mut context = Context::from_waker(&waker);
    let mut next = Box::pin(receiver.next());
    assert!(matches!(next.as_mut().poll(&mut context), Poll::Pending));
    drop(sender);
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    assert!(matches!(
        next.as_mut().poll(&mut context),
        Poll::Ready(Ok(None))
    ));
}

/// 队列中的取消请求被取走时仍保持取消，不能让迟到回复冒充成功。
#[test]
fn queued_request_remembers_a_dropped_caller() {
    let (sender, mut receiver) = plugin_ui_queue::<(), ()>(1).unwrap();
    let reply = sender.try_send(()).unwrap();
    drop(reply);
    let request = ready(receiver.next()).unwrap().unwrap();
    assert!(request.is_cancelled());
    assert!(!request.respond(()));
    assert!(sender.try_send(()).is_ok());
}

/// 实例关闭只撤销自己的调用；同窗口的其他实例继续使用原队列。
#[test]
fn closing_one_instance_does_not_cancel_another() {
    let routes = PluginUiRoutes::<(&str, u64), &str, u32>::new();
    let (sender, mut receiver) = plugin_ui_queue(4).unwrap();
    let mut first = routes.register(("first", 1), sender.clone()).unwrap();
    let _second = routes.register(("second", 1), sender).unwrap();
    let first_reply = routes.try_send(&("first", 1), "first call").unwrap();
    let second_reply = routes.try_send(&("second", 1), "second call").unwrap();
    let first_request = ready(receiver.next()).unwrap().unwrap();
    let second_request = ready(receiver.next()).unwrap().unwrap();
    first.close();
    assert_eq!(ready(first_reply), Err(PluginUiQueueError::Closed));
    assert!(first_request.is_cancelled());
    assert!(!second_request.is_cancelled());
    assert!(second_request.respond(12));
    assert_eq!(ready(second_reply), Ok(12));
    assert!(matches!(
        routes.try_send(&("first", 1), "late"),
        Err(PluginUiQueueError::MissingRoute)
    ));
}

/// 不同窗口按登记身份接收请求；丢失身份不会回退到另一个窗口。
#[test]
fn routes_never_redirect_an_instance_to_another_window() {
    let routes = PluginUiRoutes::<(&str, u64), u32, u32>::new();
    let (left_sender, mut left) = plugin_ui_queue(2).unwrap();
    let (right_sender, mut right) = plugin_ui_queue(2).unwrap();
    let _left = routes.register(("panel", 1), left_sender).unwrap();
    let _right = routes.register(("panel", 2), right_sender).unwrap();
    let left_reply = routes.try_send(&("panel", 1), 10).unwrap();
    let right_reply = routes.try_send(&("panel", 2), 20).unwrap();
    let mut left_request = ready(left.next()).unwrap().unwrap();
    let mut right_request = ready(right.next()).unwrap().unwrap();
    assert_eq!(left_request.take_message(), Some(10));
    assert_eq!(right_request.take_message(), Some(20));
    assert!(left_request.respond(11));
    assert!(right_request.respond(21));
    assert_eq!(ready(left_reply), Ok(11));
    assert_eq!(ready(right_reply), Ok(21));
    assert!(matches!(
        routes.try_send(&("panel", 3), 30),
        Err(PluginUiQueueError::MissingRoute)
    ));
}

/// 重复登记不能覆盖原窗口，迟到的旧句柄释放也不能取消重开后的路由。
#[test]
fn instance_registration_is_unique_and_old_guard_cannot_remove_new_route() {
    let routes = PluginUiRoutes::<(&str, u64), u32, u32>::new();
    let (old_sender, _old_receiver) = plugin_ui_queue(2).unwrap();
    let (new_sender, mut new_receiver) = plugin_ui_queue(2).unwrap();
    let mut old = routes.register(("panel", 1), old_sender).unwrap();
    assert!(matches!(
        routes.register(("panel", 1), new_sender.clone()),
        Err(PluginUiQueueError::DuplicateRoute)
    ));
    old.close();
    let _new = routes.register(("panel", 1), new_sender).unwrap();
    drop(old);
    let reply = routes.try_send(&("panel", 1), 7).unwrap();
    let mut request = ready(new_receiver.next()).unwrap().unwrap();
    assert_eq!(request.take_message(), Some(7));
    assert!(request.respond(8));
    assert_eq!(ready(reply), Ok(8));
}

/// 关闭与后台入队竞争时，已受理的请求被取消，后续请求不回退到别的实例。
#[test]
fn instance_close_racing_with_submit_cancels_every_accepted_request() {
    let routes = PluginUiRoutes::<u64, usize, usize>::new();
    let (sender, mut receiver) = plugin_ui_queue(16).unwrap();
    let mut guard = routes.register(7, sender).unwrap();
    let barrier = Arc::new(Barrier::new(9));
    let mut threads = Vec::new();
    for index in 0..8 {
        let routes = routes.clone();
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            routes.try_send(&7, index)
        }));
    }
    barrier.wait();
    guard.close();
    let mut accepted = 0;
    for thread in threads {
        match thread.join().unwrap() {
            Ok(reply) => {
                accepted += 1;
                assert_eq!(ready(reply), Err(PluginUiQueueError::Closed));
            }
            Err(error) => assert_eq!(error, PluginUiQueueError::MissingRoute),
        }
    }
    for _ in 0..accepted {
        let request = ready(receiver.next()).unwrap().unwrap();
        assert!(request.is_cancelled());
    }
    assert!(matches!(
        routes.try_send(&7, 9),
        Err(PluginUiQueueError::MissingRoute)
    ));
}

/// 关闭实例只取消回复；窗口回收旧请求前，其资源仍计入共享队列容量。
#[test]
fn closed_instance_keeps_in_flight_capacity_until_window_releases_request() {
    let routes = PluginUiRoutes::<u64, (), ()>::new();
    let (sender, mut receiver) = plugin_ui_queue(1).unwrap();
    let mut first = routes.register(1, sender.clone()).unwrap();
    let _second = routes.register(2, sender).unwrap();
    let first_reply = routes.try_send(&1, ()).unwrap();
    let first_request = ready(receiver.next()).unwrap().unwrap();
    first.close();
    assert_eq!(ready(first_reply), Err(PluginUiQueueError::Closed));
    assert!(matches!(
        routes.try_send(&2, ()),
        Err(PluginUiQueueError::Full)
    ));
    drop(first_request);
    assert!(routes.try_send(&2, ()).is_ok());
}

/// 取消先取得门禁时，即使窗口稍后尝试提交，也不能执行任何副作用。
#[test]
fn cancelling_before_commit_prevents_the_side_effect() {
    let (sender, mut receiver) = plugin_ui_queue::<(), u32>(1).unwrap();
    let mut reply = sender.try_send(()).unwrap();
    let request = ready(receiver.next()).unwrap().unwrap();
    let calls = AtomicUsize::new(0);

    assert_eq!(reply.cancel_if_pending(), Ok(None));
    assert!(request.is_cancelled());
    assert!(!request.complete_with(|| {
        calls.fetch_add(1, Ordering::SeqCst);
        17
    }));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(sender.try_send(()).is_ok());
}

/// 同步提交先完成时，迟到的取消必须取得已生效的结果。
#[test]
fn committing_before_cancellation_preserves_the_result() {
    let (sender, mut receiver) = plugin_ui_queue::<(), u32>(1).unwrap();
    let mut reply = sender.try_send(()).unwrap();
    let request = ready(receiver.next()).unwrap().unwrap();
    let calls = AtomicUsize::new(0);

    assert!(request.complete_with(|| {
        calls.fetch_add(1, Ordering::SeqCst);
        17
    }));
    assert_eq!(reply.cancel_if_pending(), Ok(Some(17)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// 并发取消和提交只能有一个胜者；已经生效的副作用必须伴随对应回复。
#[test]
fn cancellation_and_commit_have_one_consistent_winner() {
    for _ in 0..100 {
        let (sender, mut receiver) = plugin_ui_queue::<(), u32>(1).unwrap();
        let mut reply = sender.try_send(()).unwrap();
        let request = ready(receiver.next()).unwrap().unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_barrier = Arc::clone(&barrier);
        let worker_calls = Arc::clone(&calls);
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            request.complete_with(|| {
                worker_calls.fetch_add(1, Ordering::SeqCst);
                17
            })
        });
        barrier.wait();
        let cancellation = reply.cancel_if_pending();
        let committed = worker.join().unwrap();
        match cancellation {
            Ok(Some(17)) => {
                assert!(committed);
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            }
            Ok(None) => {
                assert!(!committed);
                assert_eq!(calls.load(Ordering::SeqCst), 0);
            }
            other => panic!("unexpected cancellation result: {other:?}"),
        }
    }
}

/// 路由撤销后即使旧请求的完成锁仍被占用，新实例也不能让旧副作用重新生效。
#[test]
fn reopening_a_route_cannot_commit_an_old_in_flight_request() {
    let routes = PluginUiRoutes::<u64, (), u32>::new();
    let (sender, mut receiver) = plugin_ui_queue(2).unwrap();
    let mut old_guard = routes.register(7, sender.clone()).unwrap();
    let reply = routes.try_send(&7, ()).unwrap();
    let request = ready(receiver.next()).unwrap().unwrap();
    let completion = Arc::clone(request.completion.as_ref().unwrap());
    let held = completion.state.lock().unwrap();
    let closing = std::thread::spawn(move || old_guard.close());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while routes.entries.lock().unwrap().contains_key(&7) {
        if std::time::Instant::now() >= deadline {
            drop(held);
            closing.join().unwrap();
            panic!("route close did not revoke its registration");
        }
        std::thread::yield_now();
    }
    let _new_guard = routes.register(7, sender).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let worker_calls = Arc::clone(&calls);
    let (done, result) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let accepted = request.complete_with(|| {
            worker_calls.fetch_add(1, Ordering::SeqCst);
            17
        });
        done.send(accepted).unwrap();
    });
    let before_unlock = result.recv_timeout(std::time::Duration::from_secs(2));
    drop(held);
    closing.join().unwrap();
    worker.join().unwrap();
    assert!(!before_unlock.unwrap());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(ready(reply), Err(PluginUiQueueError::Closed));
}

impl<M, R> PluginUiRequest<M, R> {
    /// 只有调用方仍在等待且窗口未关闭时才允许开始尚未提交的 UI 副作用。
    pub(crate) fn is_cancelled(&self) -> bool {
        let Some(completion) = &self.completion else {
            return true;
        };
        if completion
            .registration
            .as_ref()
            .is_some_and(|registration| registration.lock().map_or(true, |active| !*active))
        {
            return true;
        }
        completion
            .state
            .lock()
            .map_or(true, |state| !state.caller_alive || state.finished)
    }

    /// 弹层可异步等待调用方放弃或窗口关闭，并及时撤销尚未提交的交互。
    pub(crate) async fn cancelled(&self) {
        poll_fn(|cx| {
            let Some(completion) = &self.completion else {
                return Poll::Ready(());
            };
            if completion
                .registration
                .as_ref()
                .is_some_and(|registration| registration.lock().map_or(true, |active| !*active))
            {
                return Poll::Ready(());
            }
            let mut state = match completion.state.lock() {
                Ok(state) => state,
                Err(_) => return Poll::Ready(()),
            };
            if !state.caller_alive || state.finished {
                return Poll::Ready(());
            }
            replace_waker(&mut state.cancelled_waker, cx.waker());
            Poll::Pending
        })
        .await
    }

    /// 已完成的副作用应立即回复；已关闭调用方时返回 false，不冒充插件收到了结果。
    pub(crate) fn respond(mut self, reply: R) -> bool {
        let Some(completion) = self.completion.take() else {
            return false;
        };
        let accepted = completion.respond(reply);
        completion.retired.store(true, Ordering::Release);
        accepted
    }
}

impl<R> Completion<R> {
    /// 实例门禁与回复锁共同仲裁关闭和结果，迟到回调不能覆盖先前完成的结果。
    fn respond(&self, reply: R) -> bool {
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
        if state.finished {
            return false;
        }
        let accepted = state.caller_alive;
        if accepted {
            state.reply = Some(reply);
        }
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
        accepted
    }
}
