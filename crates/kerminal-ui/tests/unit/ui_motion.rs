//! 右栏目标数据区入场动效的回归：首次落位、迟到结果、快速切换与停止条件。
//!
//! @author kongweiguang

use std::time::{Duration, Instant};

use super::{DATA_ENTRANCE_OPACITY, TRANSITION_DURATION, TargetDataEntrance};

/// 浮点采样按固定精度比较，避免缓出曲线在端点附近产生无意义抖动。
fn assert_close(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() < 1e-5,
        "expected {expected}, got {actual}"
    );
}

/// 建立初始挂载基线：首个目标直接完整显示，不产生入场。
fn baseline(motion: &mut TargetDataEntrance, now: Instant) -> u64 {
    let epoch = motion.target_changed(true, true, now);
    assert!(!motion.ready(epoch, now, true));
    assert_close(motion.opacity(now), 1.0);
    assert!(!motion.is_running(now));
    epoch
}

#[test]
/// 初始挂载和同目标刷新都沿用完整画面，避免打开面板或手动刷新时反复闪动。
fn initial_mount_and_same_target_refresh_do_not_animate() {
    let start = Instant::now();
    let mut motion = TargetDataEntrance::default();
    let epoch = baseline(&mut motion, start);

    // 同目标刷新只重新读取数据，不会产生新的目标代次。
    let refreshed_at = start + Duration::from_millis(40);
    assert!(!motion.ready(epoch, refreshed_at, true));
    assert_close(motion.opacity(refreshed_at), 1.0);
    assert!(!motion.is_running(refreshed_at));
}

#[test]
/// 目标变化后的首个有效结果只播放一次，完成后退回静止画面且不再请求帧。
fn first_ready_result_after_target_change_enters_once() {
    let start = Instant::now();
    let mut motion = TargetDataEntrance::default();
    let _initial = baseline(&mut motion, start);
    let target_epoch = motion.target_changed(true, true, start);

    // 数据尚未就绪时不能提前播放，旧画面保持完整。
    assert!(!motion.is_running(start));
    assert_close(motion.opacity(start), 1.0);

    assert!(motion.ready(target_epoch, start, true));
    assert_close(motion.opacity(start), DATA_ENTRANCE_OPACITY);
    assert!(motion.is_running(start));

    let halfway = start + TRANSITION_DURATION / 2;
    let interpolated = motion.opacity(halfway);
    assert!(interpolated > DATA_ENTRANCE_OPACITY && interpolated < 1.0);

    let finished = start + TRANSITION_DURATION;
    assert_close(motion.opacity(finished), 1.0);
    assert!(!motion.is_running(finished));

    // 同一目标代次的后续结果不能重置或重放动画。
    assert!(!motion.ready(target_epoch, finished + Duration::from_millis(1), true));
    assert_close(motion.opacity(finished + Duration::from_millis(1)), 1.0);
}

#[test]
/// 旧目标的迟到结果不能消费新目标的入场机会，新代次结果仍然可以播放。
fn late_result_for_a_previous_target_is_ignored() {
    let start = Instant::now();
    let mut motion = TargetDataEntrance::default();
    let initial_epoch = baseline(&mut motion, start);
    let current_epoch = motion.target_changed(true, true, start);

    assert!(!motion.ready(initial_epoch, start, true));
    assert_close(motion.opacity(start), 1.0);
    assert!(!motion.is_running(start));

    assert!(motion.ready(current_epoch, start, true));
    assert!(motion.is_running(start));
}

#[test]
/// 快速切换时新结果从当前采样值继续收敛，不从固定起点跳变。
fn fast_target_switch_continues_from_the_current_sample() {
    let start = Instant::now();
    let mut motion = TargetDataEntrance::default();
    let _initial = baseline(&mut motion, start);
    let first_epoch = motion.target_changed(true, true, start);
    assert!(motion.ready(first_epoch, start, true));

    let switched_at = start + Duration::from_millis(70);
    let sampled_at_switch = motion.opacity(switched_at);
    assert!(sampled_at_switch > DATA_ENTRANCE_OPACITY && sampled_at_switch < 1.0);

    let second_epoch = motion.target_changed(true, true, switched_at);
    // 加载占位不属于数据入场：切换目标时立即停止旧动画，保留完整画面。
    assert_close(motion.opacity(switched_at), 1.0);
    assert!(!motion.is_running(switched_at));

    let ready_at = switched_at + Duration::from_millis(10);
    assert!(motion.ready(second_epoch, ready_at, true));
    assert_close(motion.opacity(ready_at), sampled_at_switch);
    assert_close(motion.opacity(ready_at + TRANSITION_DURATION), 1.0);
    assert!(!motion.is_running(ready_at + TRANSITION_DURATION));
}

#[test]
/// 新目标数据在上一段动画结束后才就绪时，重新使用固定的轻微起点。
fn late_ready_after_completed_entrance_restarts_from_the_fixed_opacity() {
    let start = Instant::now();
    let mut motion = TargetDataEntrance::default();
    let _initial = baseline(&mut motion, start);
    let first_epoch = motion.target_changed(true, true, start);
    assert!(motion.ready(first_epoch, start, true));

    let finished = start + TRANSITION_DURATION;
    let second_epoch = motion.target_changed(true, true, finished);
    let ready_at = finished + Duration::from_millis(5);
    assert!(motion.ready(second_epoch, ready_at, true));
    assert_close(motion.opacity(ready_at), DATA_ENTRANCE_OPACITY);
}

#[test]
/// 启动期间目标在首个结果前被重绑，仍视为初始挂载，不让第一次可见数据闪入。
fn target_rebind_before_first_result_stays_on_the_initial_mount() {
    let start = Instant::now();
    let mut motion = TargetDataEntrance::default();
    let first_epoch = motion.target_changed(true, true, start);
    let rebound_epoch = motion.target_changed(true, true, start);

    assert!(!motion.ready(first_epoch, start, true));
    assert!(!motion.ready(rebound_epoch, start, true));
    assert_close(motion.opacity(start), 1.0);
    assert!(!motion.is_running(start));

    // 首个有效结果建立基线后，再切换目标才进入入场。
    let later_epoch = motion.target_changed(true, true, start);
    assert!(motion.ready(later_epoch, start, true));
    assert!(motion.is_running(start));
}

#[test]
/// 减少动效和系统偏好关闭都立即落位；进行中的过渡也必须立刻结束。
fn reduced_motion_finishes_immediately() {
    let start = Instant::now();
    let mut motion = TargetDataEntrance::default();
    let _initial = baseline(&mut motion, start);

    // 目标变化时缓存到系统关闭动效，即使调用方误传 animate=true 也不能播放。
    let reduced_epoch = motion.target_changed(true, false, start);
    assert!(!motion.ready(reduced_epoch, start, true));
    assert_close(motion.opacity(start), 1.0);
    assert!(!motion.is_running(start));

    // 中途关闭 reduce_motion 时，settle 让当前帧直接完整并停止帧请求。
    let animated_epoch = motion.target_changed(true, true, start);
    assert!(motion.ready(animated_epoch, start, true));
    let halfway = start + TRANSITION_DURATION / 2;
    assert!(motion.is_running(halfway));
    motion.settle();
    assert_close(motion.opacity(halfway), 1.0);
    assert!(!motion.is_running(halfway));
}

#[test]
/// 隐藏会停止进行中的帧请求，但保留尚未消费的目标等待，重显后仍可入场一次。
fn hiding_stops_frames_and_preserves_a_pending_entrance() {
    let start = Instant::now();
    let mut motion = TargetDataEntrance::default();
    let _initial = baseline(&mut motion, start);

    let pending_epoch = motion.target_changed(true, true, start);
    motion.settle();
    assert!(!motion.is_running(start));
    assert_close(motion.opacity(start), 1.0);

    // 隐藏期间未调用 ready；重新显示后的首个有效结果仍可播放。
    assert!(motion.ready(pending_epoch, start, true));
    assert!(motion.is_running(start));

    let halfway = start + TRANSITION_DURATION / 2;
    motion.settle();
    assert_close(motion.opacity(halfway), 1.0);
    assert!(!motion.is_running(halfway));
}

#[test]
/// 重显同一目标只复用已消费的入场，不再闪动；只有新目标代次才重新等待。
fn re_showing_the_same_target_does_not_replay_a_consumed_entrance() {
    let start = Instant::now();
    let mut motion = TargetDataEntrance::default();
    let _initial = baseline(&mut motion, start);
    let target_epoch = motion.target_changed(true, true, start);
    assert!(motion.ready(target_epoch, start, true));

    let hidden_at = start + Duration::from_millis(40);
    motion.settle();
    assert_close(motion.opacity(hidden_at), 1.0);
    assert!(!motion.is_running(hidden_at));

    // 重新显示后的同目标刷新不能重放已消费的入场。
    let shown_at = start + Duration::from_millis(80);
    assert!(!motion.ready(target_epoch, shown_at, true));
    assert_close(motion.opacity(shown_at), 1.0);
    assert!(!motion.is_running(shown_at));

    let next_epoch = motion.target_changed(true, true, shown_at);
    assert!(motion.ready(next_epoch, shown_at, true));
    assert!(motion.is_running(shown_at));
}

#[test]
/// 首次读取失败不消费入场，重试成功仍是该目标的首个有效结果。
fn failed_first_read_keeps_the_entrance_for_a_retry() {
    let start = Instant::now();
    let mut motion = TargetDataEntrance::default();
    let _initial = baseline(&mut motion, start);
    let target_epoch = motion.target_changed(true, true, start);

    // 失败路径不调用 ready，画面保持完整且没有在途帧。
    assert!(!motion.is_running(start));
    assert_close(motion.opacity(start), 1.0);

    let retry_at = start + Duration::from_millis(30);
    assert!(motion.ready(target_epoch, retry_at, true));
    assert_close(motion.opacity(retry_at), DATA_ENTRANCE_OPACITY);
    assert_close(motion.opacity(retry_at + TRANSITION_DURATION), 1.0);
}

#[test]
/// 只有“目标变化后尚未接纳首个有效结果”的窗口需要调用方重读系统偏好；
/// 隐藏保留等待，但首次结果被接纳或目标失效后不再重读，避免普通刷新触发 SPI。
fn awaiting_result_tracks_only_unconsumed_target_changes() {
    let start = Instant::now();
    let mut motion = TargetDataEntrance::default();

    // 初始挂载不进入等待：首次可见数据不应被当成目标切换。
    let initial_epoch = motion.target_changed(true, true, start);
    assert!(!motion.awaiting_result());

    motion.ready(initial_epoch, start, true);
    let target_epoch = motion.target_changed(true, true, start);
    assert!(motion.awaiting_result());

    // 隐藏只停止帧请求，保留等待；重新显示后的首个结果仍需重查偏好。
    motion.settle();
    assert!(motion.awaiting_result());

    motion.ready(target_epoch, start, true);
    assert!(!motion.awaiting_result());

    motion.target_changed(false, true, start);
    assert!(!motion.awaiting_result());
}

#[test]
/// 就绪时的当前系统检查否决后，本次入场已消费，稍后偏好恢复也不能补播。
fn a_current_preference_denial_at_ready_does_not_replay_later() {
    let start = Instant::now();
    let mut motion = TargetDataEntrance::default();
    let _initial = baseline(&mut motion, start);

    let target_epoch = motion.target_changed(true, true, start);
    assert!(motion.awaiting_result());

    // 调用方在就绪处发现当前 SPI/GPUI 偏好不允许：只落位，不启动。
    assert!(!motion.ready(target_epoch, start, false));
    assert_close(motion.opacity(start), 1.0);
    assert!(!motion.is_running(start));
    assert!(!motion.awaiting_result());

    // 数据已经显示后再放宽偏好，不能对同一目标补播迟到的入场。
    let later = start + Duration::from_millis(10);
    assert!(!motion.ready(target_epoch, later, true));
    assert_close(motion.opacity(later), 1.0);
    assert!(!motion.is_running(later));
}

#[test]
/// 目标不可用时立即落位并作废旧代次，任何旧结果都不能再触发数据区动画。
fn unavailable_target_settles_and_rejects_stale_epochs() {
    let start = Instant::now();
    let mut motion = TargetDataEntrance::default();
    let _initial = baseline(&mut motion, start);
    let animated_epoch = motion.target_changed(true, true, start);
    assert!(motion.ready(animated_epoch, start, true));

    let unavailable_at = start + Duration::from_millis(20);
    let unavailable_epoch = motion.target_changed(false, true, unavailable_at);
    assert_close(motion.opacity(unavailable_at), 1.0);
    assert!(!motion.is_running(unavailable_at));
    assert!(!motion.ready(unavailable_epoch, unavailable_at, true));
    assert!(!motion.ready(animated_epoch, unavailable_at, true));
    assert_close(motion.opacity(unavailable_at), 1.0);
}
