//! 工作台动效的反向、中断和无动画状态回归。
//!
//! @author kongweiguang

use super::{MotionTargets, TRANSITION_DURATION, WorkbenchMotion};
use std::time::{Duration, Instant};

/// 统一的宽窗口基线使测试只改变目标面板，不依赖真实时钟或系统配置。
fn expanded() -> MotionTargets {
    MotionTargets {
        sidebar: true,
        dock: true,
        host_drawer: false,
        tool_drawer: false,
        compact_width: false,
        compact_panels: false,
    }
}

#[test]
/// 快速反向必须沿用当前采样值，并且其它面板不跟着开关闪动。
fn reversal_continues_from_the_current_visual_value() {
    let start = Instant::now();
    let mut motion = WorkbenchMotion::default();
    let initial = expanded();
    let frame = motion.update_at(initial, start, true);
    assert_eq!((frame.sidebar, frame.dock), (1.0, 1.0));

    let closed = MotionTargets {
        sidebar: false,
        ..initial
    };
    let first = motion.update_at(closed, start, true);
    assert_eq!(first.sidebar, 1.0);
    let halfway = start + TRANSITION_DURATION / 2;
    let before_reverse = motion.update_at(closed, halfway, true);
    assert!(before_reverse.sidebar > 0.0 && before_reverse.sidebar < 1.0);
    let reversed = motion.update_at(initial, halfway, true);
    assert_eq!(reversed.sidebar, before_reverse.sidebar);
    assert_eq!(reversed.dock, 1.0);
    let finished = motion.update_at(initial, halfway + TRANSITION_DURATION, true);
    assert_eq!(finished.sidebar, 1.0);
}

#[test]
/// 关闭动画偏好必须立刻提交最终画面，也应中断已经开始的过渡。
fn reduced_motion_finishes_an_in_flight_transition_immediately() {
    let start = Instant::now();
    let mut motion = WorkbenchMotion::default();
    let initial = expanded();
    motion.update_at(initial, start, true);
    let closed = MotionTargets {
        dock: false,
        ..initial
    };
    motion.update_at(closed, start, true);
    let snapped = motion.update_at(closed, start + Duration::from_millis(20), false);
    assert_eq!(snapped.dock, 0.0);
    assert!(!motion.transitions.unwrap()[1].is_running(start + Duration::from_millis(20)));
    let reopened = motion.update_at(initial, start + Duration::from_millis(30), false);
    assert_eq!(reopened.dock, 1.0);
}

#[test]
/// 拖动窗口跨越紧凑断点时，旧布局和新抽屉均立即落位，避免额外的尺寸波动。
fn responsive_breakpoint_changes_do_not_start_panel_transitions() {
    let start = Instant::now();
    let mut motion = WorkbenchMotion::default();
    let initial = expanded();
    motion.update_at(initial, start, true);
    let compact = MotionTargets {
        sidebar: false,
        dock: false,
        host_drawer: true,
        tool_drawer: false,
        compact_width: true,
        compact_panels: true,
    };
    let frame = motion.update_at(compact, start + Duration::from_millis(1), true);
    assert_eq!(
        (
            frame.sidebar,
            frame.dock,
            frame.host_drawer,
            frame.tool_drawer
        ),
        (0.0, 0.0, 1.0, 0.0)
    );
    assert!(
        !motion
            .transitions
            .unwrap()
            .into_iter()
            .any(|state| state.is_running(start + Duration::from_millis(1)))
    );
}

#[test]
/// 抽屉切换方向使用相互独立的进度，旧内容卸载后新抽屉可立即拥有完整焦点。
fn switching_drawers_does_not_reuse_the_old_drawer_progress() {
    let start = Instant::now();
    let mut motion = WorkbenchMotion::default();
    let host = MotionTargets {
        sidebar: false,
        dock: false,
        host_drawer: true,
        tool_drawer: false,
        compact_width: true,
        compact_panels: true,
    };
    motion.update_at(host, start, true);
    let tool = MotionTargets {
        host_drawer: false,
        tool_drawer: true,
        ..host
    };
    let switching = motion.update_at(tool, start, true);
    assert_eq!((switching.host_drawer, switching.tool_drawer), (1.0, 0.0));
    let finished = motion.update_at(tool, start + TRANSITION_DURATION, true);
    assert_eq!((finished.host_drawer, finished.tool_drawer), (0.0, 1.0));
}
