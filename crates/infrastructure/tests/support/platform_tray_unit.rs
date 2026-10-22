//! 托盘菜单映射与内嵌图标结构测试，不依赖登录桌面会话。  
//!
//! @author kongweiguang

use super::{
    HIDE_ID, MAX_EVENTS_PER_SOURCE_PER_TURN, QUIT_ID, SHOW_ID, kerminal_tray_icon,
    run_event_bridge, tray_action_for_menu_id,
};
use kerminal_application::platform::TrayAction;
use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc as std_mpsc,
    },
    thread,
    time::Duration,
};

/// 构造无需桌面线程的持续非空事件流，并用预算保证失败实现也能自行收尾。
fn fake_continuous_events(event_budget: usize) -> impl FnMut() -> Option<()> {
    let mut remaining = event_budget;
    move || {
        if remaining == 0 {
            None
        } else {
            remaining -= 1;
            Some(())
        }
    }
}

/// 固定的 Show/Hide/Quit 菜单 ID 映射到唯一应用动作，未知事件不会传播。
#[test]
fn known_menu_ids_map_to_desktop_actions() {
    assert_eq!(tray_action_for_menu_id(SHOW_ID), Some(TrayAction::Show));
    assert_eq!(tray_action_for_menu_id(HIDE_ID), Some(TrayAction::Hide));
    assert_eq!(tray_action_for_menu_id(QUIT_ID), Some(TrayAction::Quit));
    assert_eq!(tray_action_for_menu_id("third-party.action"), None);
}

/// 托盘图标的 RGBA 缓冲区尺寸固定为 32×32，避免资源损坏导致 native builder 崩溃。
#[test]
fn embedded_icon_has_valid_rgba_dimensions() {
    assert!(kerminal_tray_icon().is_ok());
}

/// 队列满时事件桥必须背压，释放容量后仍交付原退出动作而不静默丢弃。
#[test]
fn full_queue_backpressures_tray_action_until_capacity_is_available() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    sender
        .try_send(TrayAction::Hide)
        .expect("fill bounded tray queue");
    let start = std::sync::Arc::new(Barrier::new(2));
    let worker_start = std::sync::Arc::clone(&start);
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let sender_finished = sender.clone();
    let (finished, completion) = std_mpsc::channel();
    let worker = thread::spawn(move || {
        worker_start.wait();
        super::dispatch_tray_action(&sender_finished, &worker_stop, TrayAction::Quit);
        let _ = finished.send(());
    });

    start.wait();
    assert!(matches!(
        completion.recv_timeout(Duration::from_millis(20)),
        Err(std_mpsc::RecvTimeoutError::Timeout)
    ));
    assert_eq!(receiver.try_recv(), Ok(TrayAction::Hide));
    completion
        .recv_timeout(Duration::from_secs(1))
        .expect("event bridge resumes after bounded capacity is released");
    worker.join().expect("tray event bridge worker");
    assert_eq!(receiver.try_recv(), Ok(TrayAction::Quit));
}

/// 连续非空的伪托盘队列在收到 stop 后必须打断当前 drain 并完成桥线程退出。
#[test]
fn continuous_event_source_observes_stop_and_exits() {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let (finished, completion) = std_mpsc::channel();
    let worker = thread::spawn(move || {
        let handled = AtomicUsize::new(0);
        run_event_bridge(
            &worker_stop,
            fake_continuous_events(4_096),
            |_| {
                if handled.fetch_add(1, Ordering::Relaxed) + 1 == 5 {
                    worker_stop.store(true, Ordering::Release);
                }
            },
            fake_continuous_events(4_096),
            |_| panic!("stop raised during tray batch must skip the menu batch"),
        );
        let _ = finished.send(handled.load(Ordering::Relaxed));
    });

    assert_eq!(
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("continuous event bridge must observe stop promptly"),
        5
    );
    worker.join().expect("tray event bridge worker");
}

/// 托盘事件持续到达时仍须在一个有界批次后处理菜单事件，并响应菜单侧触发的停止。
#[test]
fn continuous_tray_events_do_not_starve_menu_events() {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let (finished, completion) = std_mpsc::channel();
    let worker = thread::spawn(move || {
        let tray_handled = AtomicUsize::new(0);
        let menu_handled = AtomicUsize::new(0);
        run_event_bridge(
            &worker_stop,
            fake_continuous_events(4_096),
            |_| {
                tray_handled.fetch_add(1, Ordering::Relaxed);
            },
            fake_continuous_events(4_096),
            |_| {
                let handled = menu_handled.fetch_add(1, Ordering::Relaxed) + 1;
                if handled == MAX_EVENTS_PER_SOURCE_PER_TURN {
                    worker_stop.store(true, Ordering::Release);
                }
            },
        );
        let _ = finished.send((
            tray_handled.load(Ordering::Relaxed),
            menu_handled.load(Ordering::Relaxed),
        ));
    });

    assert_eq!(
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("both continuous event sources must make progress"),
        (
            MAX_EVENTS_PER_SOURCE_PER_TURN,
            MAX_EVENTS_PER_SOURCE_PER_TURN
        )
    );
    worker.join().expect("tray event bridge worker");
}
