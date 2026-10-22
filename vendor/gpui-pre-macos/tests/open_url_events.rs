// @author kongweiguang

#[path = "../src/open_url_events.rs"]
mod open_url_events;

use open_url_events::OpenUrlEventQueue;

/// 验证 handler 安装前到达的多个批次会按顺序各交付一次。
#[test]
fn early_url_batches_are_drained_in_order_once() {
    let mut events = OpenUrlEventQueue::default();
    events.enqueue(vec!["kerminal://first".to_string()]);
    events.enqueue(vec![
        "kerminal://second".to_string(),
        "kerminal://third".to_string(),
    ]);

    assert_eq!(
        events.begin_dispatch(),
        Some(vec!["kerminal://first".to_string()])
    );
    assert_eq!(
        events.next_batch(),
        Some(vec![
            "kerminal://second".to_string(),
            "kerminal://third".to_string()
        ])
    );
    assert_eq!(events.next_batch(), None);
    assert_eq!(events.begin_dispatch(), None);
}

/// 验证回调重入期间新来的事件排在已接收批次之后，不会并行启动第二次投递。
#[test]
fn reentrant_url_batches_follow_the_existing_queue() {
    let mut events = OpenUrlEventQueue::default();
    events.enqueue(vec!["kerminal://first".to_string()]);
    events.enqueue(vec!["kerminal://second".to_string()]);

    assert_eq!(
        events.begin_dispatch(),
        Some(vec!["kerminal://first".to_string()])
    );
    events.enqueue(vec!["kerminal://reentrant".to_string()]);

    assert_eq!(events.begin_dispatch(), None);
    assert_eq!(
        events.next_batch(),
        Some(vec!["kerminal://second".to_string()])
    );
    assert_eq!(
        events.next_batch(),
        Some(vec!["kerminal://reentrant".to_string()])
    );
    assert_eq!(events.next_batch(), None);
}
