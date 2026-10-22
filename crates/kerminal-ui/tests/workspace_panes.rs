//! 分屏视图比例映射的外部契约测试，不启动窗口或触碰终端会话。
//!
//! @author kongweiguang

use gpui_kit::px;
use kerminal_ui::features::workspace_panes::normalize_child_sizes;

/// 拖动尺寸必须按稳定 child ID 归一化，布局比例不能依赖映射容器的排序。
#[test]
fn resize_sizes_keep_stable_child_identity_and_normalize_to_one() {
    let child_ids = vec![
        "pane-a".to_owned(),
        "split-b".to_owned(),
        "pane-c".to_owned(),
    ];
    let sizes = normalize_child_sizes(&child_ids, &[px(100.), px(300.), px(600.)])
        .expect("三项正尺寸应生成可持久化比例");

    assert!((sizes["pane-a"] - 0.1).abs() < 1e-9);
    assert!((sizes["split-b"] - 0.3).abs() < 1e-9);
    assert!((sizes["pane-c"] - 0.6).abs() < 1e-9);
    assert!((sizes.values().sum::<f64>() - 1.0).abs() < 1e-9);
}

/// 未布局、重复键或无穷尺寸不能落入持久化比例，避免产生无法恢复的 split。
#[test]
fn resize_sizes_reject_invalid_snapshots() {
    let ids = vec!["pane-a".to_owned(), "pane-b".to_owned()];
    assert!(normalize_child_sizes(&ids, &[px(100.)]).is_none());
    assert!(normalize_child_sizes(&ids, &[px(0.), px(200.)]).is_none());
    assert!(normalize_child_sizes(&ids, &[px(f32::INFINITY), px(200.)]).is_none());

    let duplicate_ids = vec!["same".to_owned(), "same".to_owned()];
    assert!(normalize_child_sizes(&duplicate_ids, &[px(100.), px(200.)]).is_none());
}
