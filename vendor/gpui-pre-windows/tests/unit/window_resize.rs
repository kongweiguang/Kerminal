// @author kongweiguang

use super::PendingWindowResize;
use gpui::{DevicePixels, size};

/// 一帧内的尺寸突发只提交最终尺寸，避免将中间交换链状态呈现给用户。
#[test]
fn coalesces_size_burst_before_presentation() {
    let state = PendingWindowResize::default();
    let first = size(DevicePixels(900), DevicePixels(600));
    let middle = size(DevicePixels(1100), DevicePixels(700));
    let latest = size(DevicePixels(1400), DevicePixels(900));

    state.queue(first);
    state.queue(middle);
    state.queue(latest);

    assert!(!state.may_present());
    assert_eq!(state.take_ready(), Some(latest));
    assert!(state.may_present());
    assert_eq!(state.take_ready(), None);
}

/// 最小化不消耗待处理尺寸；恢复后的事件覆盖旧值，保证最终客户区尺寸生效。
#[test]
fn minimize_preserves_work_until_restore_supplies_current_size() {
    let state = PendingWindowResize::default();
    state.queue(size(DevicePixels(900), DevicePixels(600)));
    state.set_minimized(true);
    assert_eq!(state.take_ready(), None);
    assert!(!state.may_present());

    let restored = size(DevicePixels(1200), DevicePixels(800));
    state.set_minimized(false);
    state.queue(restored);
    assert_eq!(state.take_ready(), Some(restored));
    assert!(state.may_present());
}

/// 设备失败时保留较新的重入尺寸，并在重建前禁止消费和呈现。
#[test]
fn failed_resize_waits_for_recovery_and_keeps_latest_reentrant_size() {
    let state = PendingWindowResize::default();
    let attempted = size(DevicePixels(900), DevicePixels(600));
    let latest = size(DevicePixels(1600), DevicePixels(900));
    state.queue(attempted);
    assert_eq!(state.take_ready(), Some(attempted));
    state.queue(latest);
    state.failed(attempted);

    assert_eq!(state.take_ready(), None);
    assert!(!state.may_present());
    state.recovered();
    assert_eq!(state.take_ready(), Some(latest));
    assert!(state.may_present());
}

/// 没有更新尺寸时，失败的尺寸本身必须在设备恢复后重试。
#[test]
fn failed_resize_is_retried_after_recovery() {
    let state = PendingWindowResize::default();
    let attempted = size(DevicePixels(900), DevicePixels(600));
    state.failed(attempted);
    assert_eq!(state.take_ready(), None);
    state.recovered();
    assert_eq!(state.take_ready(), Some(attempted));
}
