//! 会话队列保证单会话顺序、跨会话并发和关闭/广播撤销语义。
//!
//! @author kongweiguang

use kerminal_application::sessions::{UiInputProjection, UiSessionBinding, UiTerminalInputRequest};
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::SessionOwner,
};
use kerminal_terminal::input::{
    InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyEvent,
    TerminalMouseAction, TerminalMouseButton, TerminalMouseEvent,
};

use crate::root::session_queue::{
    InputOrigin, SessionCommandQueues, SessionQueueError, SessionQueueLimits, SessionWork,
    SnapshotRequestGate, UiInputWork, WheelInputBatch, is_broadcastable_input,
};

fn limits(concurrent_sessions: usize) -> SessionQueueLimits {
    SessionQueueLimits {
        per_session_commands: 8,
        total_commands: 16,
        total_input_bytes: 128,
        concurrent_sessions,
    }
}

/// 测试绑定模拟经过校验的可见 UI pane，固定 owner 与 generation 以覆盖广播 CAS 行为。
fn ui_binding(session_id: SessionId) -> UiSessionBinding {
    UiSessionBinding {
        session_id,
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
        session_generation: 7,
    }
}

/// 创建带精确 pane binding 的协议滚轮事件，让队列测试直接覆盖输入批次准入。
///
/// @author kongweiguang
fn wheel_work(binding: &UiSessionBinding, event: TerminalInputEvent) -> UiInputWork {
    UiInputWork::new_wheel(UiTerminalInputRequest {
        binding: binding.clone(),
        expected_context: None,
        projection: UiInputProjection::Invalidates,
        event,
    })
}

/// 每个鼠标步保留自身位置和修饰键，适用于连续但指针位置有变化的滚动报告。
///
/// @author kongweiguang
fn wheel_mouse(column: u16, row: u16) -> TerminalInputEvent {
    TerminalInputEvent::Mouse(TerminalMouseEvent {
        button: TerminalMouseButton::WheelUp,
        action: TerminalMouseAction::Scroll,
        button_down: false,
        column,
        row,
        modifiers: InputModifiers::default(),
    })
}

/// 同会话输入保持 FIFO，释放并发名额后按 ready 队列公平服务更早等待的 lane。
/// 两个会话可先并行运行；满额时阻塞，完成后逐项核对调度身份、文本和队列字节预算。
///
/// @author kongweiguang
#[test]
fn queues_are_fifo_per_session_and_parallel_between_sessions() {
    let mut queues = SessionCommandQueues::with_limits(limits(2));
    let first = SessionId::new();
    let second = SessionId::new();
    queues
        .enqueue(
            first.clone(),
            SessionWork::Input {
                event: TerminalInputEvent::Text("a".to_owned()),
                origin: InputOrigin::Direct,
            },
        )
        .unwrap();
    queues
        .enqueue(
            first.clone(),
            SessionWork::Input {
                event: TerminalInputEvent::Text("b".to_owned()),
                origin: InputOrigin::Direct,
            },
        )
        .unwrap();
    queues
        .enqueue(
            second.clone(),
            SessionWork::Input {
                event: TerminalInputEvent::Text("other".to_owned()),
                origin: InputOrigin::Direct,
            },
        )
        .unwrap();
    assert_eq!((queues.queued_commands, queues.queued_input_bytes), (3, 7));

    let first_work = queues.take_ready().unwrap();
    assert_eq!(first_work.session_id, first);
    assert!(matches!(
        &first_work.work,
        SessionWork::Input {
            event: TerminalInputEvent::Text(text),
            ..
        } if text == "a"
    ));
    assert_eq!((queues.queued_commands, queues.queued_input_bytes), (2, 6));
    let second_work = queues.take_ready().unwrap();
    assert_eq!(second_work.session_id, second);
    assert!(matches!(
        &second_work.work,
        SessionWork::Input {
            event: TerminalInputEvent::Text(text),
            ..
        } if text == "other"
    ));
    assert_eq!((queues.queued_commands, queues.queued_input_bytes), (1, 1));

    let third = SessionId::new();
    let fourth = SessionId::new();
    let fifth = SessionId::new();
    for session_id in [&third, &fourth, &fifth] {
        queues
            .enqueue(
                session_id.clone(),
                SessionWork::Input {
                    event: TerminalInputEvent::Text("capacity".to_owned()),
                    origin: InputOrigin::Direct,
                },
            )
            .unwrap();
    }
    assert_eq!((queues.queued_commands, queues.queued_input_bytes), (4, 25));
    assert!(queues.take_ready().is_none());
    assert_eq!((queues.queued_commands, queues.queued_input_bytes), (4, 25));

    assert!(queues.complete(&first_work.session_id));
    let third_work = queues.take_ready().unwrap();
    assert_eq!(third_work.session_id, third);
    assert!(matches!(
        &third_work.work,
        SessionWork::Input {
            event: TerminalInputEvent::Text(text),
            ..
        } if text == "capacity"
    ));
    assert_eq!((queues.queued_commands, queues.queued_input_bytes), (3, 17));
    assert!(queues.take_ready().is_none());

    assert!(queues.complete(&second_work.session_id));
    let fourth_work = queues.take_ready().unwrap();
    assert_eq!(fourth_work.session_id, fourth);
    assert!(matches!(
        &fourth_work.work,
        SessionWork::Input {
            event: TerminalInputEvent::Text(text),
            ..
        } if text == "capacity"
    ));
    assert_eq!((queues.queued_commands, queues.queued_input_bytes), (2, 9));

    assert!(queues.complete(&third_work.session_id));
    let fifth_work = queues.take_ready().unwrap();
    assert_eq!(fifth_work.session_id, fifth);
    assert!(matches!(
        &fifth_work.work,
        SessionWork::Input {
            event: TerminalInputEvent::Text(text),
            ..
        } if text == "capacity"
    ));
    assert_eq!((queues.queued_commands, queues.queued_input_bytes), (1, 1));

    assert!(queues.complete(&fourth_work.session_id));
    let next_first = queues.take_ready().unwrap();
    assert_eq!(next_first.session_id, first_work.session_id);
    assert!(matches!(
        &next_first.work,
        SessionWork::Input {
            event: TerminalInputEvent::Text(text),
            ..
        } if text == "b"
    ));
    assert_eq!((queues.queued_commands, queues.queued_input_bytes), (0, 0));

    assert!(queues.complete(&fifth_work.session_id));
    assert!(queues.complete(&next_first.session_id));
    assert!(queues.take_ready().is_none());
    assert_eq!((queues.queued_commands, queues.queued_input_bytes), (0, 0));
}

/// 突发滚轮按有界批次复用 FIFO 槽位，事件字节仍逐项计费，取消摘要反映全部未发输入。
///
/// @author kongweiguang
#[test]
fn wheel_bursts_are_batched_without_exceeding_count_or_byte_budgets() {
    let binding = ui_binding(SessionId::new());
    let mut queues = SessionCommandQueues::with_limits(SessionQueueLimits {
        per_session_commands: 2,
        total_commands: 2,
        total_input_bytes: 16 * 1024,
        concurrent_sessions: 1,
    });

    for index in 0..512_u16 {
        queues
            .enqueue_wheel_input(
                binding.session_id.clone(),
                wheel_work(&binding, wheel_mouse(index, index / 2)),
            )
            .unwrap();
    }

    assert_eq!(queues.queued_commands, 2);
    assert_eq!(queues.queued_input_bytes, 16 * 1024);
    let pending = &queues.lanes[&binding.session_id].pending;
    assert_eq!(pending.len(), 2);
    for (batch, start) in pending.iter().zip([0, 256]) {
        let SessionWork::WheelInputBatch(batch) = batch else {
            panic!("wheel input must use a bounded batch");
        };
        assert_eq!(batch.events.len(), 256);
        assert_eq!(batch.binding, binding);
        assert_eq!(batch.events.first(), Some(&wheel_mouse(start, start / 2)));
        let last = start + 255;
        assert_eq!(batch.events.last(), Some(&wheel_mouse(last, last / 2)));
    }
    assert_eq!(
        queues.enqueue_wheel_input(
            binding.session_id.clone(),
            wheel_work(&binding, wheel_mouse(512, 256)),
        ),
        Err(SessionQueueError::QueueFull)
    );

    let dropped = queues.cancel_pending(&binding.session_id);
    assert_eq!(dropped.commands, 2);
    assert_eq!(dropped.input_events, 512);
    assert_eq!(dropped.input_bytes, 16 * 1024);
    assert_eq!((queues.queued_commands, queues.queued_input_bytes), (0, 0));
}

/// 只有滚轮专属标记可合批；普通方向键会打断批次，并继续按原始 FIFO 顺序写入。
///
/// @author kongweiguang
#[test]
fn wheel_batches_keep_order_around_an_ordinary_arrow_key() {
    let binding = ui_binding(SessionId::new());
    let mut queues = SessionCommandQueues::with_limits(limits(1));
    queues
        .enqueue_wheel_input(
            binding.session_id.clone(),
            wheel_work(&binding, wheel_mouse(2, 3)),
        )
        .unwrap();
    queues
        .enqueue_wheel_input(
            binding.session_id.clone(),
            wheel_work(&binding, wheel_mouse(4, 5)),
        )
        .unwrap();
    let ordinary_up = TerminalInputEvent::Key(TerminalKeyEvent {
        key: TerminalKey::Up,
        phase: KeyPhase::Press,
        modifiers: InputModifiers::default(),
    });
    queues
        .enqueue(
            binding.session_id.clone(),
            SessionWork::UiInput(Box::new(UiInputWork::new(
                UiTerminalInputRequest {
                    binding: binding.clone(),
                    expected_context: None,
                    projection: UiInputProjection::Invalidates,
                    event: ordinary_up.clone(),
                },
                None,
            ))),
        )
        .unwrap();
    queues
        .enqueue_wheel_input(
            binding.session_id.clone(),
            wheel_work(&binding, wheel_mouse(6, 7)),
        )
        .unwrap();

    let first = queues.take_ready().expect("first wheel batch is ready");
    let SessionWork::WheelInputBatch(WheelInputBatch {
        binding: first_binding,
        events,
    }) = first.work
    else {
        panic!("first item should be a wheel batch");
    };
    assert_eq!(first_binding, binding);
    assert_eq!(events, [wheel_mouse(2, 3), wheel_mouse(4, 5)]);
    assert!(queues.complete(&binding.session_id));

    let second = queues
        .take_ready()
        .expect("ordinary key stays between wheel batches");
    let SessionWork::UiInput(work) = second.work else {
        panic!("ordinary key must keep its original UI input item");
    };
    assert_eq!(work.request.binding, binding);
    assert_eq!(work.request.event, ordinary_up);
    assert!(!work.wheel_input);
    assert!(queues.complete(&binding.session_id));

    let third = queues
        .take_ready()
        .expect("second wheel batch remains last");
    let SessionWork::WheelInputBatch(WheelInputBatch {
        binding: last_binding,
        events,
    }) = third.work
    else {
        panic!("last item should be a wheel batch");
    };
    assert_eq!(last_binding, binding);
    assert_eq!(events, [wheel_mouse(6, 7)]);
    assert!(queues.complete(&binding.session_id));
    assert!(queues.take_ready().is_none());
}

/// 会话 ID 相同但 generation 改变时必须开启新批次，迟到输入不能跨重连绑定。
///
/// @author kongweiguang
#[test]
fn wheel_batches_do_not_cross_session_generations() {
    let original = ui_binding(SessionId::new());
    let reconnected = UiSessionBinding {
        session_id: original.session_id.clone(),
        owner: original.owner.clone(),
        session_generation: original.session_generation + 1,
    };
    let mut queues = SessionCommandQueues::with_limits(limits(1));
    queues
        .enqueue_wheel_input(
            original.session_id.clone(),
            wheel_work(&original, wheel_mouse(1, 1)),
        )
        .unwrap();
    queues
        .enqueue_wheel_input(
            reconnected.session_id.clone(),
            wheel_work(&reconnected, wheel_mouse(2, 2)),
        )
        .unwrap();

    let pending = &queues.lanes[&original.session_id].pending;
    assert_eq!(pending.len(), 2);
    for (work, expected_binding) in pending.iter().zip([&original, &reconnected]) {
        let SessionWork::WheelInputBatch(batch) = work else {
            panic!("each generation should own its wheel batch");
        };
        assert_eq!(&batch.binding, expected_binding);
        assert_eq!(batch.events.len(), 1);
    }
}

/// 替代滚动方向键可以批量串行写出，但相反方向仍逐项保留原顺序。
///
/// @author kongweiguang
#[test]
fn alternate_scroll_batches_preserve_each_arrow_direction() {
    let binding = ui_binding(SessionId::new());
    let mut queues = SessionCommandQueues::with_limits(limits(1));
    let key = |key| {
        TerminalInputEvent::Key(TerminalKeyEvent {
            key,
            phase: KeyPhase::Press,
            modifiers: InputModifiers::default(),
        })
    };
    for event in [key(TerminalKey::Up), key(TerminalKey::Down)] {
        queues
            .enqueue_wheel_input(binding.session_id.clone(), wheel_work(&binding, event))
            .unwrap();
    }

    let ready = queues.take_ready().expect("alternate wheel batch is ready");
    let SessionWork::WheelInputBatch(batch) = ready.work else {
        panic!("alternate scroll uses the wheel batch path");
    };
    assert_eq!(batch.binding, binding);
    assert_eq!(batch.events, [key(TerminalKey::Up), key(TerminalKey::Down)]);
    assert!(queues.complete(&ready.session_id));
}

/// 接受计划的 CAS 下一步优先于后来排队的输入，但仍要等当前写入 ACK 后发送。
#[test]
fn enqueue_front_preserves_lane_ownership_and_idle_gate() {
    let mut queues = SessionCommandQueues::with_limits(limits(1));
    let session = SessionId::new();
    assert!(queues.is_idle(&session));

    queues
        .enqueue(
            session.clone(),
            SessionWork::Input {
                event: TerminalInputEvent::Text("first".to_owned()),
                origin: InputOrigin::Direct,
            },
        )
        .unwrap();
    let in_flight = queues.take_ready().unwrap();
    assert!(!queues.is_idle(&session));

    queues
        .enqueue(
            session.clone(),
            SessionWork::Input {
                event: TerminalInputEvent::Text("typed later".to_owned()),
                origin: InputOrigin::Direct,
            },
        )
        .unwrap();
    queues
        .enqueue_front(
            session.clone(),
            SessionWork::Input {
                event: TerminalInputEvent::Text("accepted edit".to_owned()),
                origin: InputOrigin::Direct,
            },
        )
        .unwrap();

    assert!(queues.complete(&in_flight.session_id));
    let accepted_edit = queues.take_ready().unwrap();
    assert!(matches!(
        accepted_edit.work,
        SessionWork::Input {
            event: TerminalInputEvent::Text(ref text),
            ..
        } if text == "accepted edit"
    ));
    assert!(queues.complete(&accepted_edit.session_id));
    let typed_later = queues.take_ready().unwrap();
    assert!(matches!(
        typed_later.work,
        SessionWork::Input {
            event: TerminalInputEvent::Text(ref text),
            ..
        } if text == "typed later"
    ));
    assert!(queues.complete(&typed_later.session_id));
    assert!(queues.is_idle(&session));
}

/// 关闭撤销 pending 项但不假装 in-flight 已被取消，并用实际出队状态验证清理。
///
/// @author kongweiguang
#[test]
fn cancel_pending_reports_drops_without_claiming_in_flight_work_was_cancelled() {
    let mut queues = SessionCommandQueues::with_limits(SessionQueueLimits {
        per_session_commands: 1,
        total_commands: 1,
        total_input_bytes: 32,
        concurrent_sessions: 1,
    });
    let session = SessionId::new();
    queues
        .enqueue(
            session.clone(),
            SessionWork::Input {
                event: TerminalInputEvent::Text("in-flight".to_owned()),
                origin: InputOrigin::Direct,
            },
        )
        .unwrap();
    let in_flight = queues.take_ready().unwrap();
    queues
        .enqueue(
            session.clone(),
            SessionWork::Input {
                event: TerminalInputEvent::Text("pending".to_owned()),
                origin: InputOrigin::Direct,
            },
        )
        .unwrap();
    assert_eq!(
        queues.enqueue(
            session.clone(),
            SessionWork::Input {
                event: TerminalInputEvent::Text("overflow".to_owned()),
                origin: InputOrigin::Direct,
            },
        ),
        Err(SessionQueueError::QueueFull)
    );
    let dropped = queues.cancel_pending(&session);
    assert_eq!(dropped.commands, 1);
    assert_eq!(dropped.input_events, 1);
    assert_eq!(dropped.input_bytes, "pending".len());
    assert_eq!((queues.queued_commands, queues.queued_input_bytes), (0, 0));
    assert!(queues.take_ready().is_none());
    assert!(queues.complete(&in_flight.session_id));
    assert!(queues.take_ready().is_none());
}

/// 退休清除待发送历史 resize 时精确计数，并保留仍执行中的网格目标直到回执。
///
/// @author kongweiguang
#[test]
fn history_resize_cancellation_preserves_only_the_in_flight_target() {
    let mut queues = SessionCommandQueues::with_limits(limits(1));
    let session_id = SessionId::new();
    let binding = ui_binding(session_id.clone());
    let in_flight_size = kerminal_domain::sessions::TerminalSize::new(80, 24, 800, 480).unwrap();
    let pending_size = kerminal_domain::sessions::TerminalSize::new(100, 30, 1000, 600).unwrap();

    queues
        .enqueue(
            session_id.clone(),
            SessionWork::HistoryResize {
                binding: binding.clone(),
                size: in_flight_size,
            },
        )
        .unwrap();
    let in_flight = queues.take_ready().unwrap();
    queues
        .enqueue(
            session_id.clone(),
            SessionWork::HistoryResize {
                binding,
                size: pending_size,
            },
        )
        .unwrap();

    let dropped = queues.cancel_pending(&session_id);
    assert_eq!(dropped.history_resize_commands, 1);
    assert_eq!(dropped.commands, 1);
    assert_eq!(
        queues.in_flight_history_resize_size(&session_id),
        Some(in_flight_size)
    );
    assert!(queues.complete(&in_flight.session_id));
    assert_eq!(queues.in_flight_history_resize_size(&session_id), None);
}

/// 退出回收未发送 viewport work，但释放旧 in-flight 后同一会话仍可继续只读滚动。
///
/// @author kongweiguang
#[test]
fn pending_viewport_work_is_cancelled_without_retiring_the_history_lane() {
    let mut queues = SessionCommandQueues::with_limits(limits(1));
    let session = SessionId::new();
    let binding = ui_binding(session.clone());
    queues
        .enqueue(
            session.clone(),
            SessionWork::Scroll {
                binding: binding.clone(),
                lines: -2,
            },
        )
        .unwrap();
    let _in_flight = queues.take_ready().expect("first viewport update starts");
    queues
        .enqueue(
            session.clone(),
            SessionWork::ScrollTo {
                binding: binding.clone(),
                offset: 23,
            },
        )
        .unwrap();

    let dropped = queues.cancel_pending(&session);
    assert_eq!(dropped.commands, 1);
    assert_eq!((dropped.input_events, dropped.input_bytes), (0, 0));
    assert!(queues.take_ready().is_none());
    assert!(queues.complete(&session));

    queues
        .enqueue(
            session.clone(),
            SessionWork::ScrollTo {
                binding: binding.clone(),
                offset: 7,
            },
        )
        .expect("read-only history keeps an enqueueable FIFO after cleanup");
    assert!(matches!(
        queues.take_ready().unwrap().work,
        SessionWork::ScrollTo { offset: 7, .. }
    ));
    assert!(queues.complete(&session));
}

/// 取消同會話尚未发送的工作后，另一会话仍能继续调度。
#[test]
fn pending_cancellation_preserves_other_session_scheduling() {
    let mut queues = SessionCommandQueues::with_limits(limits(2));
    let closing = SessionId::new();
    let other = SessionId::new();
    queues
        .enqueue(
            closing.clone(),
            SessionWork::Input {
                event: TerminalInputEvent::Text("write".to_owned()),
                origin: InputOrigin::Direct,
            },
        )
        .unwrap();
    let in_flight = queues.take_ready().unwrap();
    queues
        .enqueue(
            closing.clone(),
            SessionWork::Input {
                event: TerminalInputEvent::Text("must drop".to_owned()),
                origin: InputOrigin::Direct,
            },
        )
        .unwrap();
    let dropped = queues.cancel_pending(&closing);
    assert_eq!(dropped.input_events, 1);

    queues
        .enqueue(
            other.clone(),
            SessionWork::Scroll {
                binding: ui_binding(other.clone()),
                lines: 1,
            },
        )
        .unwrap();
    let other_work = queues.take_ready().unwrap();
    assert_eq!(other_work.session_id, other);
    assert!(queues.take_ready().is_none());

    assert!(queues.take_ready().is_none());
    queues.complete(&in_flight.session_id);
    queues
        .enqueue(
            closing.clone(),
            SessionWork::Scroll {
                binding: ui_binding(closing.clone()),
                lines: 1,
            },
        )
        .unwrap();
    let next = queues.take_ready().unwrap();
    assert_eq!(next.session_id, closing);
    queues.complete(&next.session_id);
    queues.complete(&other_work.session_id);
}

/// 邻接的同代滚动可合并，重连后的 binding 必须保留为独立 FIFO 项。
///
/// @author kongweiguang
#[test]
fn scrolls_only_coalesce_for_the_same_complete_binding() {
    let mut queues = SessionCommandQueues::with_limits(limits(1));
    let session_id = SessionId::new();
    let binding = ui_binding(session_id.clone());
    let next_generation = UiSessionBinding {
        session_id: session_id.clone(),
        owner: binding.owner.clone(),
        session_generation: binding.session_generation + 1,
    };

    for (binding, lines) in [
        (binding.clone(), 1),
        (binding.clone(), 2),
        (next_generation.clone(), 4),
    ] {
        queues
            .enqueue(session_id.clone(), SessionWork::Scroll { binding, lines })
            .unwrap();
    }

    let first = queues.take_ready().unwrap();
    assert_eq!(
        first.work,
        SessionWork::Scroll {
            binding: binding.clone(),
            lines: 3,
        }
    );
    assert!(queues.complete(&session_id));
    let second = queues.take_ready().unwrap();
    assert_eq!(
        second.work,
        SessionWork::Scroll {
            binding: next_generation,
            lines: 4,
        }
    );
    assert!(queues.complete(&session_id));
}

/// 绝对滚动只合并相邻同 binding 的目标，保留换代请求的 FIFO 顺序。
///
/// @author kongweiguang
#[test]
fn absolute_scrolls_keep_the_latest_offset_without_crossing_binding_generations() {
    let mut queues = SessionCommandQueues::with_limits(limits(1));
    let session_id = SessionId::new();
    let binding = ui_binding(session_id.clone());
    let next_generation = UiSessionBinding {
        session_id: session_id.clone(),
        owner: binding.owner.clone(),
        session_generation: binding.session_generation + 1,
    };

    for (binding, offset) in [
        (binding.clone(), 5),
        (binding.clone(), 12),
        (next_generation.clone(), 2),
    ] {
        queues
            .enqueue(
                session_id.clone(),
                SessionWork::ScrollTo { binding, offset },
            )
            .unwrap();
    }

    let first = queues.take_ready().unwrap();
    assert_eq!(
        first.work,
        SessionWork::ScrollTo {
            binding: binding.clone(),
            offset: 12,
        }
    );
    assert!(queues.complete(&session_id));
    let second = queues.take_ready().unwrap();
    assert_eq!(
        second.work,
        SessionWork::ScrollTo {
            binding: next_generation,
            offset: 2,
        }
    );
    assert!(queues.complete(&session_id));
}

/// SelectionScroll 只合并相同 binding 与手势，并保留最后提交的 offset/request_id。
///
/// @author kongweiguang
#[test]
fn selection_scroll_coalesces_by_binding_and_gesture_with_latest_request_id() {
    let mut queues = SessionCommandQueues::with_limits(limits(1));
    let session_id = SessionId::new();
    let binding = ui_binding(session_id.clone());
    let next_generation = UiSessionBinding {
        session_id: session_id.clone(),
        owner: binding.owner.clone(),
        session_generation: binding.session_generation + 1,
    };

    for (binding, offset, gesture_id, request_id) in [
        (binding.clone(), 5, 11, 1),
        (binding.clone(), 12, 11, 2),
        (binding.clone(), 17, 12, 3),
        (next_generation.clone(), 2, 11, 4),
    ] {
        queues
            .enqueue(
                session_id.clone(),
                SessionWork::SelectionScroll {
                    binding,
                    offset,
                    gesture_id,
                    request_id,
                },
            )
            .unwrap();
    }
    assert_eq!(queues.queued_commands, 3);

    let first = queues.take_ready().unwrap();
    assert_eq!(
        first.work,
        SessionWork::SelectionScroll {
            binding: binding.clone(),
            offset: 12,
            gesture_id: 11,
            request_id: 2,
        }
    );
    assert!(queues.complete(&session_id));
    let second = queues.take_ready().unwrap();
    assert_eq!(
        second.work,
        SessionWork::SelectionScroll {
            binding: binding.clone(),
            offset: 17,
            gesture_id: 12,
            request_id: 3,
        }
    );
    assert!(queues.complete(&session_id));
    let third = queues.take_ready().unwrap();
    assert_eq!(
        third.work,
        SessionWork::SelectionScroll {
            binding: next_generation,
            offset: 2,
            gesture_id: 11,
            request_id: 4,
        }
    );
    assert!(queues.complete(&session_id));
}

/// 相邻 resize 仅在类型与完整 binding 均相同时合并，历史网格 work 不覆盖 PTY resize。
///
/// @author kongweiguang
#[test]
fn resize_work_coalesces_only_for_the_same_kind_and_binding() {
    let mut queues = SessionCommandQueues::with_limits(limits(1));
    let session_id = SessionId::new();
    let binding = ui_binding(session_id.clone());
    let next_generation = UiSessionBinding {
        session_id: session_id.clone(),
        owner: binding.owner.clone(),
        session_generation: binding.session_generation + 1,
    };
    let size_a = kerminal_domain::sessions::TerminalSize::new(80, 24, 800, 480).unwrap();
    let size_b = kerminal_domain::sessions::TerminalSize::new(100, 30, 1000, 600).unwrap();
    let size_c = kerminal_domain::sessions::TerminalSize::new(120, 36, 1200, 720).unwrap();

    for work in [
        SessionWork::Resize {
            binding: binding.clone(),
            size: size_a,
        },
        SessionWork::Resize {
            binding: binding.clone(),
            size: size_b,
        },
        SessionWork::HistoryResize {
            binding: binding.clone(),
            size: size_a,
        },
        SessionWork::HistoryResize {
            binding: binding.clone(),
            size: size_c,
        },
        SessionWork::HistoryResize {
            binding: next_generation.clone(),
            size: size_b,
        },
    ] {
        queues.enqueue(session_id.clone(), work).unwrap();
    }
    assert_eq!(queues.queued_commands, 3);

    let live = queues.take_ready().unwrap();
    assert_eq!(
        live.work,
        SessionWork::Resize {
            binding: binding.clone(),
            size: size_b,
        }
    );
    assert!(queues.complete(&session_id));
    let history = queues.take_ready().unwrap();
    assert_eq!(
        history.work,
        SessionWork::HistoryResize {
            binding: binding.clone(),
            size: size_c,
        }
    );
    assert!(queues.complete(&session_id));
    let next_generation_work = queues.take_ready().unwrap();
    assert_eq!(
        next_generation_work.work,
        SessionWork::HistoryResize {
            binding: next_generation,
            size: size_b,
        }
    );
    assert!(queues.complete(&session_id));
}

/// Stop 只撤销固定 generation 的广播输入，保留并真实调度同会话直接输入。
///
/// @author kongweiguang
#[test]
fn broadcast_generation_invalidates_stale_validation_and_preserves_direct_work() {
    let mut queues = SessionCommandQueues::with_limits(limits(1));
    let session = SessionId::new();
    let binding = ui_binding(session.clone());
    let (first_generation, _) = queues.begin_broadcast_start();
    assert!(queues.broadcast_start_is_current(first_generation));
    let (generation, _) = queues.begin_broadcast_start();
    assert!(!queues.activate_broadcast(first_generation, vec![binding.clone()]));
    assert!(queues.activate_broadcast(generation, vec![binding.clone()]));
    queues
        .enqueue_broadcast_event(generation, TerminalInputEvent::Text("broadcast".to_owned()))
        .unwrap();
    queues
        .enqueue(
            session.clone(),
            SessionWork::Input {
                event: TerminalInputEvent::Text("direct".to_owned()),
                origin: InputOrigin::Direct,
            },
        )
        .unwrap();

    let dropped = queues.stop_broadcast();
    assert_eq!(dropped.broadcast_events, 1);
    assert_eq!(dropped.input_events, 1);
    assert_eq!(dropped.input_bytes, "broadcast".len());
    assert_eq!(
        (queues.queued_commands, queues.queued_input_bytes),
        (1, "direct".len())
    );
    assert!(!queues.broadcast_start_is_current(generation));
    let direct = queues.take_ready().expect("direct input remains scheduled");
    assert_eq!(
        direct.work,
        SessionWork::Input {
            event: TerminalInputEvent::Text("direct".to_owned()),
            origin: InputOrigin::Direct,
        }
    );
    assert!(queues.complete(&session));
    assert!(queues.take_ready().is_none());
    assert_eq!((queues.queued_commands, queues.queued_input_bytes), (0, 0));
    assert_eq!(
        queues.enqueue_broadcast_event(generation, TerminalInputEvent::Text("stale".to_owned())),
        Err(SessionQueueError::BroadcastInactive)
    );
}

/// 通过真实广播入口验证容量不足时没有任何目标收到部分工作。
///
/// @author kongweiguang
#[test]
fn broadcast_event_is_admitted_atomically_for_all_targets() {
    let mut queues = SessionCommandQueues::with_limits(SessionQueueLimits {
        per_session_commands: 4,
        total_commands: 1,
        total_input_bytes: 64,
        concurrent_sessions: 2,
    });
    let first = SessionId::new();
    let second = SessionId::new();
    let (generation, _) = queues.begin_broadcast_start();
    assert!(queues.activate_broadcast(generation, vec![ui_binding(first), ui_binding(second)]));

    assert_eq!(
        queues.enqueue_broadcast_event(generation, TerminalInputEvent::Text("x".to_owned())),
        Err(SessionQueueError::QueueFull)
    );
    assert!(queues.take_ready().is_none());
    assert_eq!((queues.queued_commands, queues.queued_input_bytes), (0, 0));
    let dropped = queues.stop_broadcast();
    assert_eq!(dropped.broadcast_events, 0);
    assert_eq!(dropped.input_events, 0);
    assert_eq!(dropped.input_bytes, 0);
}

/// 真实广播出队项保留激活时捕获的 owner、generation 和 CAS 投影策略。
///
/// @author kongweiguang
#[test]
fn broadcast_event_keeps_fixed_ui_binding_in_ui_input_request() {
    let mut queues = SessionCommandQueues::with_limits(limits(2));
    let binding = ui_binding(SessionId::new());
    let (generation, _) = queues.begin_broadcast_start();
    assert!(queues.activate_broadcast(generation, vec![binding.clone()]));

    assert_eq!(
        queues.enqueue_broadcast_event(generation, TerminalInputEvent::Text("go".to_owned())),
        Ok(1)
    );
    let ready = queues.take_ready().expect("broadcast request is queued");
    let SessionWork::UiInput(work) = ready.work else {
        panic!("broadcast work must use the owner-bound UI CAS path");
    };

    assert_eq!(work.origin, InputOrigin::Broadcast(generation));
    assert_eq!(work.acceptance, None);
    assert_eq!(work.command_line_ticket, None);
    assert_eq!(work.request.binding, binding);
    assert_eq!(work.request.expected_context, None);
    assert_eq!(
        work.request.projection,
        kerminal_application::sessions::UiInputProjection::Invalidates
    );
    assert_eq!(
        work.request.event,
        TerminalInputEvent::Text("go".to_owned())
    );
}

/// 固定目标 one-shot 在总命令数或输入字节预算不足时整批拒绝，不留下部分 FIFO 项。
///
/// @author kongweiguang
#[test]
fn broadcast_once_preflights_command_and_input_byte_budgets_atomically() {
    let first = ui_binding(SessionId::new());
    let second = ui_binding(SessionId::new());
    let bindings = [first, second];
    let event = TerminalInputEvent::Text("x".to_owned());

    for queue_limits in [
        SessionQueueLimits {
            per_session_commands: 4,
            total_commands: 1,
            total_input_bytes: 64,
            concurrent_sessions: 2,
        },
        SessionQueueLimits {
            per_session_commands: 4,
            total_commands: 4,
            total_input_bytes: 1,
            concurrent_sessions: 2,
        },
    ] {
        let mut queues = SessionCommandQueues::with_limits(queue_limits);
        assert_eq!(
            queues.enqueue_broadcast_once(&bindings, event.clone()),
            Err(SessionQueueError::QueueFull)
        );
        assert!(queues.take_ready().is_none());
        assert_eq!((queues.queued_commands, queues.queued_input_bytes), (0, 0));
    }
}

/// One-shot 使用调用时冻结的 UI binding，并在每个目标自己的 FIFO 中排于既有输入之后。
///
/// @author kongweiguang
#[test]
fn broadcast_once_keeps_fixed_bindings_and_per_session_fifo_order() {
    let mut queues = SessionCommandQueues::with_limits(limits(1));
    let first = ui_binding(SessionId::new());
    let second = ui_binding(SessionId::new());
    queues
        .enqueue(
            first.session_id.clone(),
            SessionWork::Input {
                event: TerminalInputEvent::Text("before".to_owned()),
                origin: InputOrigin::Direct,
            },
        )
        .unwrap();

    assert_eq!(
        queues.enqueue_broadcast_once(
            &[first.clone(), second.clone()],
            TerminalInputEvent::Text("once".to_owned()),
        ),
        Ok(2)
    );

    let prior = queues
        .take_ready()
        .expect("pre-existing FIFO item starts first");
    assert_eq!(prior.session_id, first.session_id);
    assert_eq!(
        prior.work,
        SessionWork::Input {
            event: TerminalInputEvent::Text("before".to_owned()),
            origin: InputOrigin::Direct,
        }
    );
    assert!(queues.complete(&first.session_id));

    // The second session was already ready; each one-shot request must retain its original owner.
    for expected in [&second, &first] {
        let ready = queues.take_ready().expect("one-shot target remains queued");
        assert_eq!(&ready.session_id, &expected.session_id);
        let SessionWork::UiInput(work) = ready.work else {
            panic!("one-shot input must use the owner-bound UI CAS path");
        };
        assert_eq!(work.origin, InputOrigin::Direct);
        assert_eq!(work.request.binding, *expected);
        assert_eq!(
            work.request.event,
            TerminalInputEvent::Text("once".to_owned())
        );
        assert!(queues.complete(&ready.session_id));
    }
    assert!(queues.take_ready().is_none());
}

/// Stop 只移除带 generation 的持续广播；显式 one-shot 仍按原 binding 排队执行。
///
/// @author kongweiguang
#[test]
fn stop_broadcast_preserves_explicit_one_shot_input() {
    let mut queues = SessionCommandQueues::with_limits(limits(1));
    let binding = ui_binding(SessionId::new());
    let (generation, _) = queues.begin_broadcast_start();
    assert!(queues.activate_broadcast(generation, vec![binding.clone()]));
    assert_eq!(
        queues.enqueue_broadcast_event(
            generation,
            TerminalInputEvent::Text("continuous".to_owned())
        ),
        Ok(1)
    );
    assert_eq!(
        queues.enqueue_broadcast_once(
            std::slice::from_ref(&binding),
            TerminalInputEvent::Text("once".to_owned())
        ),
        Ok(1)
    );

    let dropped = queues.stop_broadcast();
    assert_eq!(dropped.broadcast_events, 1);
    assert_eq!(dropped.input_events, 1);
    assert_eq!(dropped.input_bytes, "continuous".len());
    assert_eq!((queues.queued_commands, queues.queued_input_bytes), (1, 4));

    let ready = queues
        .take_ready()
        .expect("explicit one-shot survives Stop");
    assert_eq!(ready.session_id, binding.session_id);
    let SessionWork::UiInput(work) = ready.work else {
        panic!("one-shot input must remain an owner-bound UI request");
    };
    assert_eq!(work.origin, InputOrigin::Direct);
    assert_eq!(work.request.binding, binding);
    assert_eq!(
        work.request.event,
        TerminalInputEvent::Text("once".to_owned())
    );
    assert!(queues.complete(&ready.session_id));
    assert_eq!((queues.queued_commands, queues.queued_input_bytes), (0, 0));
}

/// 实际广播入口拒绝仅属于单 pane 的 focus 与 mouse，同时允许配对键盘释放。
///
/// @author kongweiguang
#[test]
fn broadcast_accepts_keyboard_text_paste_but_rejects_focus_and_mouse() {
    let mut queues = SessionCommandQueues::with_limits(limits(2));
    let binding = ui_binding(SessionId::new());
    let (generation, _) = queues.begin_broadcast_start();
    assert!(queues.activate_broadcast(generation, vec![binding.clone()]));

    let key_release = TerminalInputEvent::Key(TerminalKeyEvent {
        key: TerminalKey::Enter,
        phase: KeyPhase::Release,
        modifiers: InputModifiers::default(),
    });
    let mouse = TerminalInputEvent::Mouse(TerminalMouseEvent {
        button: TerminalMouseButton::Left,
        action: TerminalMouseAction::Press,
        button_down: true,
        column: 3,
        row: 2,
        modifiers: InputModifiers::default(),
    });
    let focus = TerminalInputEvent::Focus(true);

    assert!(is_broadcastable_input(&key_release));
    assert!(!is_broadcastable_input(&mouse));
    assert!(!is_broadcastable_input(&focus));
    assert_eq!(
        queues.enqueue_broadcast_event(generation, key_release),
        Ok(1)
    );
    assert_eq!(
        queues.enqueue_broadcast_event(generation, mouse),
        Err(SessionQueueError::BroadcastInputNotSupported)
    );
    assert_eq!(
        queues.enqueue_broadcast_event(generation, focus),
        Err(SessionQueueError::BroadcastInputNotSupported)
    );
}

/// 快速连续输入通过真实 CAS 队列项入同一 FIFO，不捕获会推进的 prompt version。
///
/// @author kongweiguang
#[test]
fn tracked_inputs_typed_before_ack_remain_ordered_under_binding_cas() {
    let mut queues = SessionCommandQueues::with_limits(limits(1));
    let binding = ui_binding(SessionId::new());
    let make_work = |text: &str| {
        SessionWork::UiInput(Box::new(super::UiInputWork::new(
            UiTerminalInputRequest {
                binding: binding.clone(),
                expected_context: None,
                projection: UiInputProjection::Tracked,
                event: TerminalInputEvent::Text(text.to_owned()),
            },
            None,
        )))
    };

    queues
        .enqueue(binding.session_id.clone(), make_work("a"))
        .unwrap();
    let first = queues.take_ready().expect("first write starts");
    queues
        .enqueue(binding.session_id.clone(), make_work("b"))
        .unwrap();
    queues
        .enqueue(binding.session_id.clone(), make_work("c"))
        .unwrap();
    assert!(queues.take_ready().is_none());

    for expected in ["a", "b", "c"] {
        let work = match if expected == "a" {
            first.clone()
        } else {
            queues.complete(&binding.session_id);
            queues.take_ready().expect("next FIFO write starts")
        }
        .work
        {
            SessionWork::UiInput(work) => {
                let super::UiInputWork { request, .. } = *work;
                request
            }
            other => panic!("expected UI CAS input, got {other:?}"),
        };
        assert_eq!(work.binding, binding);
        assert_eq!(work.expected_context, None);
        assert_eq!(work.projection, UiInputProjection::Tracked);
        assert_eq!(work.event, TerminalInputEvent::Text(expected.to_owned()));
    }
    queues.complete(&binding.session_id);
}

/// 快照请求没有 viewport 控制字段；generation 和 revision 足以拒绝迟到与回退响应。
///
/// @author kongweiguang
#[test]
fn snapshot_gate_rejects_stale_generation_and_revision_without_viewport_input() {
    let mut gate = SnapshotRequestGate::default();
    let (old, _) = gate.begin();
    let (current, request) = gate.begin();
    assert_eq!(request.since_revision, None);
    assert!(!gate.accept(old, 5));
    assert!(gate.accept(current, 8));

    gate.invalidate();
    let (scrolled, request) = gate.begin();
    assert_eq!(request.since_revision, Some(8));
    assert!(!gate.accept(current, 9));
    assert!(!gate.accept(scrolled, 7));
    assert!(gate.accept(scrolled, 9));
}
