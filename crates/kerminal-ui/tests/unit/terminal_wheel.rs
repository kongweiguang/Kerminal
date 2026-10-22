//! GPUI 滚轮输入的小数累计与方向契约。
//!
//! @author kongweiguang

use gpui_kit::{ScrollDelta, TouchPhase, point, px};

use super::{TerminalWheelAccumulator, TerminalWheelSteps};

/// 像素 delta 要跨事件累计，避免触控板每个小幅移动都被独立舍入成零。
#[test]
fn pixel_deltas_accumulate_independently_by_axis() {
    let mut accumulator = TerminalWheelAccumulator::default();
    let first = ScrollDelta::Pixels(point(px(5.0), px(4.0)));
    let second = ScrollDelta::Pixels(point(px(5.0), px(4.0)));

    assert_eq!(
        accumulator.push(&first, TouchPhase::Moved, px(10.0)),
        TerminalWheelSteps::default()
    );
    assert_eq!(
        accumulator.push(&second, TouchPhase::Moved, px(10.0)),
        TerminalWheelSteps {
            vertical: 0,
            horizontal: 1,
        }
    );
    assert_eq!(
        accumulator.push(
            &ScrollDelta::Pixels(point(px(0.0), px(2.0))),
            TouchPhase::Moved,
            px(10.0)
        ),
        TerminalWheelSteps {
            vertical: 1,
            horizontal: 0,
        }
    );
}

/// 线 delta 保持 GPUI 正负方向，并通过截零保留反向滚动的小数余量。
#[test]
fn line_deltas_preserve_direction_and_fractional_remainder() {
    let mut accumulator = TerminalWheelAccumulator::default();

    assert_eq!(
        accumulator.push(
            &ScrollDelta::Lines(point(0.0, -0.4)),
            TouchPhase::Moved,
            px(16.0),
        ),
        TerminalWheelSteps::default()
    );
    assert_eq!(
        accumulator.push(
            &ScrollDelta::Lines(point(0.0, -0.7)),
            TouchPhase::Moved,
            px(16.0),
        ),
        TerminalWheelSteps {
            vertical: -1,
            horizontal: 0,
        }
    );
}

/// Started 清除旧手势余量后仍消费首个 delta，后续 Moved 只接着累计其分数部分。
#[test]
fn started_resets_old_remainder_and_consumes_its_delta_once() {
    let mut accumulator = TerminalWheelAccumulator::default();
    accumulator.push(
        &ScrollDelta::Lines(point(0.75, 0.75)),
        TouchPhase::Moved,
        px(16.0),
    );

    assert_eq!(
        accumulator.push(
            &ScrollDelta::Lines(point(-1.25, 2.25)),
            TouchPhase::Started,
            px(16.0),
        ),
        TerminalWheelSteps {
            vertical: 2,
            horizontal: -1,
        }
    );
    assert_eq!(
        accumulator.push(
            &ScrollDelta::Lines(point(-0.75, 0.75)),
            TouchPhase::Moved,
            px(16.0),
        ),
        TerminalWheelSteps {
            vertical: 1,
            horizontal: -1,
        }
    );
    assert_eq!(
        accumulator.push(
            &ScrollDelta::Lines(point(0.0, 0.0)),
            TouchPhase::Moved,
            px(16.0),
        ),
        TerminalWheelSteps::default()
    );
}

/// 即使 Started 没有 delta，也必须清除旧余量，避免跨手势拼成一个滚动步。
#[test]
fn started_with_zero_delta_clears_old_remainder() {
    let mut accumulator = TerminalWheelAccumulator::default();
    accumulator.push(
        &ScrollDelta::Lines(point(0.0, 0.75)),
        TouchPhase::Moved,
        px(16.0),
    );

    assert_eq!(
        accumulator.push(
            &ScrollDelta::Lines(point(0.0, 0.0)),
            TouchPhase::Started,
            px(16.0),
        ),
        TerminalWheelSteps::default()
    );
    assert_eq!(
        accumulator.push(
            &ScrollDelta::Lines(point(0.0, 0.3)),
            TouchPhase::Moved,
            px(16.0),
        ),
        TerminalWheelSteps::default()
    );
    assert_eq!(
        accumulator.push(
            &ScrollDelta::Lines(point(0.0, 0.7)),
            TouchPhase::Moved,
            px(16.0),
        ),
        TerminalWheelSteps {
            vertical: 1,
            horizontal: 0,
        }
    );
}

/// 结束或取消只清除当前手势余量，不把终止事件里的 delta 发送给终端。
#[test]
fn ended_and_cancelled_events_clear_remainder_without_applying_their_delta() {
    for phase in [TouchPhase::Ended, TouchPhase::Cancelled] {
        let mut accumulator = TerminalWheelAccumulator::default();
        accumulator.push(
            &ScrollDelta::Lines(point(0.0, 0.5)),
            TouchPhase::Moved,
            px(16.0),
        );
        assert_eq!(
            accumulator.push(&ScrollDelta::Lines(point(0.0, 3.0)), phase, px(16.0)),
            TerminalWheelSteps::default()
        );
        assert_eq!(
            accumulator.push(
                &ScrollDelta::Lines(point(0.0, 0.5)),
                TouchPhase::Moved,
                px(16.0),
            ),
            TerminalWheelSteps::default()
        );
        assert_eq!(
            accumulator.push(
                &ScrollDelta::Lines(point(0.0, 0.5)),
                TouchPhase::Moved,
                px(16.0),
            ),
            TerminalWheelSteps {
                vertical: 1,
                horizontal: 0,
            }
        );
    }
}

/// 纯水平滚动只能产生水平步数，避免调用方把 y=0 错当成向上的 wheel。
#[test]
fn horizontal_delta_never_creates_vertical_scroll() {
    let mut accumulator = TerminalWheelAccumulator::default();
    let steps = accumulator.push(
        &ScrollDelta::Lines(point(-1.0, 0.0)),
        TouchPhase::Moved,
        px(16.0),
    );

    assert_eq!(
        steps,
        TerminalWheelSteps {
            vertical: 0,
            horizontal: -1,
        }
    );
}

/// 非法行高和非有限增量都不应污染后续正常输入状态。
#[test]
fn invalid_geometry_and_non_finite_delta_do_not_emit_or_corrupt_state() {
    let mut accumulator = TerminalWheelAccumulator::default();
    assert_eq!(
        accumulator.push(
            &ScrollDelta::Lines(point(0.0, 1.0)),
            TouchPhase::Moved,
            px(0.0),
        ),
        TerminalWheelSteps::default()
    );
    assert_eq!(
        accumulator.push(
            &ScrollDelta::Lines(point(f32::NAN, 1.0)),
            TouchPhase::Moved,
            px(16.0),
        ),
        TerminalWheelSteps {
            vertical: 1,
            horizontal: 0,
        }
    );
}

/// 超大输入饱和为有界步数并清掉异常余数，避免后续正常滚动长期失真。
#[test]
fn oversized_delta_saturates_without_retaining_unbounded_remainder() {
    let mut accumulator = TerminalWheelAccumulator::default();

    assert_eq!(
        accumulator.push(
            &ScrollDelta::Lines(point(0.0, f32::MAX)),
            TouchPhase::Moved,
            px(16.0),
        ),
        TerminalWheelSteps {
            vertical: i32::MAX,
            horizontal: 0,
        }
    );
    assert_eq!(
        accumulator.push(
            &ScrollDelta::Lines(point(0.0, 1.0)),
            TouchPhase::Moved,
            px(16.0),
        ),
        TerminalWheelSteps {
            vertical: 1,
            horizontal: 0,
        }
    );
}
