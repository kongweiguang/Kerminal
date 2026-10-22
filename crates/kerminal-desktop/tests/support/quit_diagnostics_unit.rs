//! 退出诊断的静态源码裁剪与同一 poll 身份回归，不创建窗口或运行后台采样。
//!
//! @author kongweiguang

use super::{ActiveTiming, is_same_poll, source_basename};

/// 构建机路径可以来自 Windows 或 Unix，诊断投影必须始终仅保留编译期文件名。
///
/// @author kongweiguang
#[test]
fn source_basename_drops_both_build_path_families() {
    assert_eq!(
        source_basename("C:\\private-build\\src\\actions.rs"),
        "actions.rs"
    );
    assert_eq!(
        source_basename("/private-build/src/actions.rs"),
        "actions.rs"
    );
    assert_eq!(source_basename("actions.rs"), "actions.rs");
}

/// 相同 spawn 位置不足以证明忙循环，只有同一活动 poll 的起点也保持不变才标记连续样本。
///
/// @author kongweiguang
#[test]
fn same_poll_requires_the_original_poll_start() {
    let timing = gpui_pre::profiler::TaskTiming::placeholder();
    let current = ActiveTiming {
        location: timing.location,
        spawned: timing.spawned,
        start: timing.start,
    };
    assert!(!is_same_poll(None, &current));
    assert!(is_same_poll(Some(current), &current));
    let later_poll = ActiveTiming {
        start: current.start + std::time::Duration::from_millis(1),
        ..current
    };
    assert!(!is_same_poll(Some(current), &later_poll));
}
