//! 将终端滚轮按 pane binding 路由到本地 scrollback 或终端鼠标协议。
//!
//! @author kongweiguang

use std::collections::HashMap;

use gpui_kit::{Pixels, ScrollDelta, TouchPhase};
use kerminal_application::sessions::{UiInputProjection, UiSessionBinding, UiTerminalInputRequest};
use kerminal_domain::{identity::SessionId, sessions::TerminalSize};
use kerminal_terminal::{
    input::{
        InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyEvent,
        TerminalMouseAction, TerminalMouseButton, TerminalMouseEvent,
    },
    snapshot::MouseTrackingMode,
};

use super::{RootView, SessionCommand};

#[path = "../terminal_wheel.rs"]
mod accumulator;

use accumulator::{TerminalWheelAccumulator, TerminalWheelSteps};

/// 单次滚轮事件允许入队的最大鼠标报告或方向键数，避免单事件占满每会话输入 FIFO。
const MAX_WHEEL_STEPS_PER_AXIS: i32 = 32;

/// Canvas 在事件发生时捕获的几何与模式；Root 仍会对照当前 pane 快照和 binding。
#[derive(Clone, Debug)]
pub(crate) struct TerminalWheelRequest {
    pub binding: UiSessionBinding,
    pub snapshot_session_id: SessionId,
    pub snapshot_size: TerminalSize,
    pub delta: ScrollDelta,
    pub phase: TouchPhase,
    pub line_height: Pixels,
    pub shift: bool,
    pub modifiers: InputModifiers,
    pub mouse_tracking: MouseTrackingMode,
    pub alternate_screen: bool,
    pub alternate_scroll: bool,
    pub application_cursor: bool,
    pub cell: Option<(u16, u16)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WheelBehaviorKey {
    mouse_tracking: MouseTrackingMode,
    alternate_screen: bool,
    alternate_scroll: bool,
    application_cursor: bool,
    shift: bool,
    line_height_bits: u32,
}

struct PaneWheelState {
    binding: UiSessionBinding,
    behavior: WheelBehaviorKey,
    accumulator: TerminalWheelAccumulator,
}

/// 每个 session 的高精度余量绑定到同一 owner/generation 和滚动策略。
#[derive(Default)]
pub(super) struct TerminalWheelState {
    panes: HashMap<SessionId, PaneWheelState>,
}

/// 同一快照下把累计后的步数路由为本地 scrollback、VT 鼠标报告、替代滚动按键或忽略。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WheelDispatch {
    Scrollback { lines: i32 },
    Mouse { vertical: i32, horizontal: i32 },
    AlternateScroll { key: TerminalKey, count: u32 },
    Ignore,
}

/// 对新的 owner、模式或字体行高清零余量，避免半步跨越重连或鼠标策略变化。
///
/// @author kongweiguang
pub(super) fn accumulate(
    state: &mut TerminalWheelState,
    request: &TerminalWheelRequest,
) -> TerminalWheelSteps {
    let behavior = WheelBehaviorKey {
        mouse_tracking: request.mouse_tracking,
        alternate_screen: request.alternate_screen,
        alternate_scroll: request.alternate_scroll,
        application_cursor: request.application_cursor,
        shift: request.shift,
        line_height_bits: request.line_height.as_f32().to_bits(),
    };
    let pane = state
        .panes
        .entry(request.binding.session_id.clone())
        .or_insert_with(|| PaneWheelState {
            binding: request.binding.clone(),
            behavior,
            accumulator: TerminalWheelAccumulator::default(),
        });
    if pane.binding != request.binding || pane.behavior != behavior {
        pane.binding = request.binding.clone();
        pane.behavior = behavior;
        pane.accumulator.reset();
    }
    pane.accumulator
        .push(&request.delta, request.phase, request.line_height)
}

/// Shift 始终保留给本地滚动；平台可能把 Shift+竖向滚轮预先改成横向 delta，故作回转。
/// 其它滚动先遵守应用鼠标模式；备用屏仅在 1007 处于启用状态时才转换为方向键。
/// 保留完整方向键步数但限制单事件最多 32 次，避免把一段滚轮增量压成一次按键或占满输入 FIFO。
///
/// @author kongweiguang
pub(super) fn dispatch(
    steps: TerminalWheelSteps,
    mouse_tracking: MouseTrackingMode,
    alternate_screen: bool,
    alternate_scroll: bool,
    shift: bool,
) -> WheelDispatch {
    if shift {
        let lines = if steps.vertical != 0 {
            steps.vertical
        } else {
            // Windows 与 Linux 的 GPUI 后端会把 Shift+竖向滚轮映射到 x 轴。
            steps.horizontal
        };
        return if lines != 0 {
            WheelDispatch::Scrollback { lines }
        } else {
            WheelDispatch::Ignore
        };
    }

    if mouse_tracking != MouseTrackingMode::None && !steps.is_empty() {
        return WheelDispatch::Mouse {
            vertical: steps
                .vertical
                .clamp(-MAX_WHEEL_STEPS_PER_AXIS, MAX_WHEEL_STEPS_PER_AXIS),
            horizontal: steps
                .horizontal
                .clamp(-MAX_WHEEL_STEPS_PER_AXIS, MAX_WHEEL_STEPS_PER_AXIS),
        };
    }
    if alternate_screen && alternate_scroll && steps.vertical != 0 {
        return WheelDispatch::AlternateScroll {
            key: if steps.vertical > 0 {
                TerminalKey::Up
            } else {
                TerminalKey::Down
            },
            count: steps
                .vertical
                .unsigned_abs()
                .min(MAX_WHEEL_STEPS_PER_AXIS as u32),
        };
    }
    if !alternate_screen && steps.vertical != 0 {
        return WheelDispatch::Scrollback {
            lines: steps.vertical,
        };
    }
    WheelDispatch::Ignore
}

/// 已退出或失败的 pane 没有 TUI 输入目标；其竖向滚轮始终读本地历史，不信任退出前残留的 VT 模式。
///
/// @author kongweiguang
fn dispatch_for_input_target(
    steps: TerminalWheelSteps,
    mouse_tracking: MouseTrackingMode,
    alternate_screen: bool,
    alternate_scroll: bool,
    shift: bool,
    has_live_input_handler: bool,
) -> WheelDispatch {
    if has_live_input_handler {
        return dispatch(
            steps,
            mouse_tracking,
            alternate_screen,
            alternate_scroll,
            shift,
        );
    }

    let lines = if shift {
        if steps.vertical != 0 {
            steps.vertical
        } else {
            steps.horizontal
        }
    } else {
        steps.vertical
    };
    if lines == 0 {
        WheelDispatch::Ignore
    } else {
        WheelDispatch::Scrollback { lines }
    }
}

/// 滚动条和历史翻页只依赖只读 view binding，退出后的历史仍可浏览；
/// TUI 鼠标与替代滚动仍要求实时输入 handler，避免向已退出的 shell 投递事件。
/// 替代滚动按有界 count 逐键进入 FIFO，遇背压即停；不自行编码协议字节。过期滚轮仍被消费以避免误滚动到父容器。
/// owner-bound work 预先盒装，
/// 让 SessionCommand 与后续 FIFO 共享同一分配，不在滚轮高频路径重复复制大请求。
///
/// @author kongweiguang
pub(crate) fn handle(
    root: &mut RootView,
    request: TerminalWheelRequest,
    cx: &mut gpui_kit::Context<RootView>,
) -> bool {
    if request.snapshot_session_id != request.binding.session_id
        || !root
            .terminal_view_bindings
            .get(&request.binding.session_id)
            .is_some_and(|binding| binding == &request.binding)
    {
        return true;
    }

    let Some(current_snapshot) = root.terminal_snapshot_for_binding(&request.binding) else {
        return true;
    };
    if current_snapshot.session_id != request.snapshot_session_id
        || current_snapshot.size != request.snapshot_size
        || current_snapshot.modes.mouse_tracking != request.mouse_tracking
        || current_snapshot.modes.alternate_screen != request.alternate_screen
        || current_snapshot.modes.alternate_scroll != request.alternate_scroll
        || current_snapshot.modes.application_cursor != request.application_cursor
    {
        return true;
    }
    let steps = accumulate(&mut root.terminal_wheels, &request);
    let has_live_input_handler = root
        .pane_input_handlers
        .get(&request.binding.session_id)
        .is_some_and(|registration| registration.binding == request.binding);
    match dispatch_for_input_target(
        steps,
        request.mouse_tracking,
        request.alternate_screen,
        request.alternate_scroll,
        request.shift,
        has_live_input_handler,
    ) {
        WheelDispatch::Scrollback { lines } => {
            if lines != 0 {
                root.enqueue_session_command(
                    SessionCommand::Scroll {
                        binding: request.binding,
                        lines,
                    },
                    cx,
                );
            }
        }
        WheelDispatch::Mouse {
            vertical,
            horizontal,
        } => {
            if !root
                .pane_input_handlers
                .get(&request.binding.session_id)
                .is_some_and(|registration| registration.binding == request.binding)
            {
                return true;
            }
            let Some((column, row)) = request.cell else {
                return true;
            };
            for (button, count) in [
                (
                    if vertical > 0 {
                        TerminalMouseButton::WheelUp
                    } else {
                        TerminalMouseButton::WheelDown
                    },
                    vertical.unsigned_abs(),
                ),
                (
                    if horizontal > 0 {
                        TerminalMouseButton::WheelRight
                    } else {
                        TerminalMouseButton::WheelLeft
                    },
                    horizontal.unsigned_abs(),
                ),
            ] {
                for _ in 0..count {
                    let accepted = root.enqueue_session_command(
                        SessionCommand::UiInput(Box::new(super::session_queue::UiInputWork::new(
                            UiTerminalInputRequest {
                                binding: request.binding.clone(),
                                expected_context: None,
                                projection: UiInputProjection::Invalidates,
                                event: TerminalInputEvent::Mouse(TerminalMouseEvent {
                                    button,
                                    action: TerminalMouseAction::Scroll,
                                    button_down: false,
                                    column,
                                    row,
                                    modifiers: request.modifiers,
                                }),
                            },
                            None,
                        ))),
                        cx,
                    );
                    if !accepted {
                        return true;
                    }
                }
            }
        }
        WheelDispatch::AlternateScroll { key, count } => {
            if !root
                .pane_input_handlers
                .get(&request.binding.session_id)
                .is_some_and(|registration| registration.binding == request.binding)
            {
                return true;
            }
            for _ in 0..count {
                let accepted = root.enqueue_session_command(
                    SessionCommand::UiInput(Box::new(super::session_queue::UiInputWork::new(
                        UiTerminalInputRequest {
                            binding: request.binding.clone(),
                            expected_context: None,
                            projection: UiInputProjection::Invalidates,
                            event: TerminalInputEvent::Key(TerminalKeyEvent {
                                key,
                                phase: KeyPhase::Press,
                                modifiers: InputModifiers::default(),
                            }),
                        },
                        None,
                    ))),
                    cx,
                );
                if !accepted {
                    return true;
                }
            }
        }
        WheelDispatch::Ignore => {}
    }
    true
}

/// 绑定失效时清掉小数余量，避免复用的 SessionId 跨 owner 接续旧手势。
///
/// @author kongweiguang
pub(super) fn remove(state: &mut TerminalWheelState, session_id: &SessionId) {
    state.panes.remove(session_id);
}

#[cfg(test)]
#[path = "../../tests/unit/root_terminal_wheel.rs"]
mod tests;
