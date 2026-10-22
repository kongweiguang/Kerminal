// @author kongweiguang

use super::SurfaceMapping;

/// 验证 remap 要等 compositor 可见 configure 和成功绘制后再进入 Mapped。
#[test]
fn remap_stays_hidden_until_a_visible_configure_and_successful_frame() {
    let mut state = SurfaceMapping::Mapped;
    assert!(state.hide());
    assert_eq!(state, SurfaceMapping::Hidden);
    assert!(state.show());
    assert_eq!(state, SurfaceMapping::Remapping);
    assert!(!state.can_render(false));
    assert!(!state.presented(false));
    assert_eq!(state, SurfaceMapping::Remapping);
    assert!(state.can_render(true));
    assert!(state.presented(true));
    assert_eq!(state, SurfaceMapping::Mapped);
    assert!(!state.presented(true));
}

/// 验证重复隐藏/显示幂等，且重新隐藏会取消尚未完成的 remap。
#[test]
fn repeated_requests_are_idempotent_and_hide_cancels_pending_remap() {
    let mut state = SurfaceMapping::Mapped;
    assert!(state.hide());
    assert!(!state.hide());
    assert!(state.show());
    assert!(!state.show());
    assert!(state.hide());
    assert_eq!(state, SurfaceMapping::Hidden);
    assert!(!state.presented(true));
    assert!(state.show());
}
