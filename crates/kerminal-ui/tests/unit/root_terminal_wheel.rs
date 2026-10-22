//! 固定 pane 滚轮余量、鼠标模式与替代屏幕的路由边界。
//!
//! @author kongweiguang

use gpui_kit::{ScrollDelta, TouchPhase, point, px};
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::SessionOwner,
};
use kerminal_terminal::{
    input::{KeyPhase, TerminalInputEvent, TerminalKeyEvent, encode_input},
    snapshot::{MouseTrackingMode, TerminalModes},
};

use crate::root::terminal_wheel::{
    TerminalWheelRequest, TerminalWheelState, WheelDispatch, accumulate, dispatch,
    dispatch_for_input_target,
};

/// 构造一份固定 pane owner，使滚动状态测试只改变明确的会话代次。
///
/// @author kongweiguang
fn binding(session_id: SessionId, generation: u64) -> UiSessionBinding {
    UiSessionBinding {
        session_id,
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
        session_generation: generation,
    }
}

/// 为状态测试构造有效度量和主屏默认策略，避免重复铺开无关事件字段。
///
/// @author kongweiguang
fn request(binding: UiSessionBinding, delta: ScrollDelta) -> TerminalWheelRequest {
    TerminalWheelRequest {
        snapshot_session_id: binding.session_id.clone(),
        binding,
        snapshot_size: Default::default(),
        delta,
        phase: TouchPhase::Moved,
        line_height: px(20.0),
        shift: false,
        modifiers: Default::default(),
        mouse_tracking: MouseTrackingMode::None,
        alternate_screen: false,
        alternate_scroll: false,
        application_cursor: false,
        cell: Some((0, 0)),
    }
}

/// 小数触控板增量必须同 pane 累计，SessionId 复用但 generation 改变时必须重新开始。
///
/// @author kongweiguang
#[test]
fn fractional_delta_does_not_cross_binding_generations() {
    let session_id = SessionId::new();
    let mut state = TerminalWheelState::default();
    let first = binding(session_id.clone(), 7);
    let next = UiSessionBinding {
        session_id,
        owner: first.owner.clone(),
        session_generation: 8,
    };

    assert_eq!(
        accumulate(
            &mut state,
            &request(first, ScrollDelta::Lines(point(0.0, 0.75)))
        )
        .vertical,
        0
    );
    assert_eq!(
        accumulate(
            &mut state,
            &request(next, ScrollDelta::Lines(point(0.0, 0.5)))
        )
        .vertical,
        0
    );
}

/// 协议模式接收鼠标滚动，Shift 仍可在主屏强制回到本地 scrollback。
///
/// @author kongweiguang
#[test]
fn tracking_and_shift_choose_distinct_routes() {
    let steps = crate::root::terminal_wheel::accumulator::TerminalWheelSteps {
        vertical: 2,
        horizontal: 1,
    };
    assert_eq!(
        dispatch(steps, MouseTrackingMode::ButtonMotion, false, false, false),
        WheelDispatch::Mouse {
            vertical: 2,
            horizontal: 1,
        }
    );
    assert_eq!(
        dispatch(steps, MouseTrackingMode::ButtonMotion, false, false, true),
        WheelDispatch::Scrollback { lines: 2 }
    );
}

/// Windows 与 Linux 会把 Shift+竖向滚轮编码到 x 轴，Root 应恢复为本地 scrollback。
///
/// @author kongweiguang
#[test]
fn shifted_platform_axis_remapping_still_scrolls_terminal_history() {
    let steps = crate::root::terminal_wheel::accumulator::TerminalWheelSteps {
        vertical: 0,
        horizontal: 3,
    };
    assert_eq!(
        dispatch(steps, MouseTrackingMode::ButtonMotion, false, false, true),
        WheelDispatch::Scrollback { lines: 3 }
    );
    assert_eq!(
        dispatch(steps, MouseTrackingMode::ButtonMotion, true, false, true),
        WheelDispatch::Scrollback { lines: 3 }
    );
}

/// 退出后的只读历史 pane 忽略 VT 残留鼠标/备用屏模式，竖向滚轮只请求本地 scrollback。
///
/// @author kongweiguang
#[test]
fn exited_history_uses_local_scrollback_despite_stale_tui_modes() {
    let steps = crate::root::terminal_wheel::accumulator::TerminalWheelSteps {
        vertical: 2,
        horizontal: 1,
    };
    assert_eq!(
        dispatch_for_input_target(
            steps,
            MouseTrackingMode::AnyMotion,
            true,
            true,
            false,
            false
        ),
        WheelDispatch::Scrollback { lines: 2 }
    );
    assert_eq!(
        dispatch_for_input_target(steps, MouseTrackingMode::AnyMotion, true, true, true, false),
        WheelDispatch::Scrollback { lines: 2 }
    );
    assert_eq!(
        dispatch_for_input_target(steps, MouseTrackingMode::AnyMotion, true, true, false, true),
        WheelDispatch::Mouse {
            vertical: 2,
            horizontal: 1,
        }
    );
}

/// alt screen 的普通滚轮遵守 TUI 模式，Shift 则始终选择本地滚动路由。
///
/// @author kongweiguang
#[test]
fn alternate_screen_never_fakes_scrollback() {
    let steps = crate::root::terminal_wheel::accumulator::TerminalWheelSteps {
        vertical: 1,
        horizontal: 0,
    };
    assert_eq!(
        dispatch(steps, MouseTrackingMode::None, true, false, false),
        WheelDispatch::Ignore
    );
    assert_eq!(
        dispatch(steps, MouseTrackingMode::AnyMotion, true, false, false),
        WheelDispatch::Mouse {
            vertical: 1,
            horizontal: 0,
        }
    );
    assert_eq!(
        dispatch(steps, MouseTrackingMode::AnyMotion, true, false, true),
        WheelDispatch::Scrollback { lines: 1 }
    );
}

/// 单个高分辨率事件的鼠标报告数有界，避免一次滚轮占满每会话输入 FIFO。
///
/// @author kongweiguang
#[test]
fn mouse_reports_are_capped_per_axis() {
    let steps = crate::root::terminal_wheel::accumulator::TerminalWheelSteps {
        vertical: i32::MAX,
        horizontal: i32::MIN,
    };
    assert_eq!(
        dispatch(steps, MouseTrackingMode::AnyMotion, false, false, false),
        WheelDispatch::Mouse {
            vertical: 32,
            horizontal: -32,
        }
    );
}

/// 不支持鼠标报告时横向滚轮不会被错误转换成纵向 scrollback。
///
/// @author kongweiguang
#[test]
fn horizontal_only_input_is_not_rewritten_as_vertical_scrollback() {
    assert_eq!(
        dispatch(
            crate::root::terminal_wheel::accumulator::TerminalWheelSteps {
                vertical: 0,
                horizontal: 3,
            },
            MouseTrackingMode::None,
            false,
            false,
            false,
        ),
        WheelDispatch::Ignore
    );
}

/// 备用滚动仅在 1007 处于启用状态且未由鼠标报告接管时转换为光标键。
///
/// @author kongweiguang
#[test]
fn alternate_scroll_requires_mode_and_yields_to_mouse_tracking() {
    let up = crate::root::terminal_wheel::accumulator::TerminalWheelSteps {
        vertical: 1,
        horizontal: 0,
    };
    assert_eq!(
        dispatch(up, MouseTrackingMode::None, true, true, false),
        WheelDispatch::AlternateScroll {
            key: kerminal_terminal::input::TerminalKey::Up,
            count: 1,
        }
    );
    assert_eq!(
        dispatch(up, MouseTrackingMode::None, true, false, false),
        WheelDispatch::Ignore
    );
    assert_eq!(
        dispatch(up, MouseTrackingMode::ButtonMotion, true, true, false),
        WheelDispatch::Mouse {
            vertical: 1,
            horizontal: 0,
        }
    );
}

/// 备用屏替代滚动保留单事件的方向和步数，方向键仍由终端统一编码器处理。
///
/// @author kongweiguang
#[test]
fn alternate_scroll_preserves_signed_multi_step_counts() {
    use kerminal_terminal::input::TerminalKey;

    for (vertical, key, count) in [(4, TerminalKey::Up, 4), (-3, TerminalKey::Down, 3)] {
        assert_eq!(
            dispatch(
                crate::root::terminal_wheel::accumulator::TerminalWheelSteps {
                    vertical,
                    horizontal: 2,
                },
                MouseTrackingMode::None,
                true,
                true,
                false,
            ),
            WheelDispatch::AlternateScroll { key, count },
        );
    }
}

/// i32 极值不会在取绝对值时溢出，单次方向键投递不超过 FIFO 保护上限，零步不入队。
///
/// @author kongweiguang
#[test]
fn alternate_scroll_caps_extreme_counts_and_ignores_zero() {
    use kerminal_terminal::input::TerminalKey;

    for (vertical, key) in [(i32::MAX, TerminalKey::Up), (i32::MIN, TerminalKey::Down)] {
        assert_eq!(
            dispatch(
                crate::root::terminal_wheel::accumulator::TerminalWheelSteps {
                    vertical,
                    horizontal: 0,
                },
                MouseTrackingMode::None,
                true,
                true,
                false,
            ),
            WheelDispatch::AlternateScroll { key, count: 32 },
        );
    }
    assert_eq!(
        dispatch(
            crate::root::terminal_wheel::accumulator::TerminalWheelSteps::default(),
            MouseTrackingMode::None,
            true,
            true,
            false,
        ),
        WheelDispatch::Ignore,
    );
}

/// 备用屏的滚动按键交给统一终端编码器，故 application_cursor 决定 CSI 或 SS3。
///
/// @author kongweiguang
#[test]
fn alternate_scroll_key_obeys_application_cursor_mode() {
    let up = crate::root::terminal_wheel::accumulator::TerminalWheelSteps {
        vertical: 1,
        horizontal: 0,
    };
    let WheelDispatch::AlternateScroll { key, count } =
        dispatch(up, MouseTrackingMode::None, true, true, false)
    else {
        panic!("alternate scroll should be a terminal cursor key");
    };
    assert_eq!(count, 1);
    for (application_cursor, expected) in [(false, b"\x1b[A".as_slice()), (true, b"\x1bOA")] {
        let modes = TerminalModes {
            alternate_screen: true,
            alternate_scroll: true,
            application_cursor,
            ..TerminalModes::default()
        };
        let event = TerminalInputEvent::Key(TerminalKeyEvent {
            key,
            phase: KeyPhase::Press,
            modifiers: Default::default(),
        });
        assert_eq!(encode_input(&event, modes).as_deref(), Some(expected));
    }
}

/// 1007 不接管横向事件；Shift 优先选择本地滚动，即使终端启用了备用屏与鼠标报告。
///
/// @author kongweiguang
#[test]
fn alternate_scroll_ignores_horizontal_while_shift_forces_local_scrollback() {
    assert_eq!(
        dispatch(
            crate::root::terminal_wheel::accumulator::TerminalWheelSteps {
                vertical: 0,
                horizontal: 2,
            },
            MouseTrackingMode::None,
            true,
            true,
            false,
        ),
        WheelDispatch::Ignore
    );
    assert_eq!(
        dispatch(
            crate::root::terminal_wheel::accumulator::TerminalWheelSteps {
                vertical: 1,
                horizontal: 0,
            },
            MouseTrackingMode::ButtonMotion,
            true,
            true,
            true,
        ),
        WheelDispatch::Scrollback { lines: 1 }
    );
}
