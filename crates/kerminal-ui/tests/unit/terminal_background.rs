//! 终端背景投影与平铺预算的边界测试。
//!
//! @author kongweiguang

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use kerminal_application::preferences::{BackgroundFit, SettingsSnapshot};

use super::{
    MAX_TILE_PAINTS, TerminalBackgroundSettings, TileAnimationState, advance_tile_animation,
    tile_plan,
};

/// 构造时固定空白路径和越界 alpha，避免路径投影探测当前目录；alpha 仍由 theme material 统一处理。
///
/// @author kongweiguang
#[test]
fn settings_projection_preserves_empty_path_and_clamps_opacity() {
    let snapshot = SettingsSnapshot {
        background_enabled: true,
        background_image_path: "  ".to_owned(),
        background_opacity: 255,
        ..SettingsSnapshot::default()
    };
    let projected = TerminalBackgroundSettings::from_snapshot(&snapshot);

    assert!(projected.enabled);
    assert_eq!(projected.image_path, None);
    assert_eq!(projected.fit, BackgroundFit::Cover);
}

/// 小图按原尺寸平铺时不改变图像比例，且能覆盖 viewport 的边缘区域。
#[test]
fn normal_tile_plan_covers_viewport_without_resizing_source_tiles() {
    let plan = tile_plan(10.0, 6.0, 3.0, 2.0).expect("positive dimensions should be drawable");

    assert_eq!(plan.tile_width, 3.0);
    assert_eq!(plan.tile_height, 2.0);
    assert_eq!(plan.columns, 4);
    assert_eq!(plan.rows, 3);
    assert!(plan.tile_width * plan.columns as f32 >= 10.0);
    assert!(plan.tile_height * plan.rows as f32 >= 6.0);
}

/// 极小图片会在保证等比的前提下放大平铺尺寸，限定每帧的图像绘制调用数。
#[test]
fn tiny_tile_plan_bounds_draw_calls_and_preserves_aspect_ratio() {
    let plan = tile_plan(1920.0, 1080.0, 1.0, 1.0).expect("positive dimensions should be drawable");

    assert!(plan.tile_width > 1.0);
    assert_eq!(plan.tile_width / plan.tile_height, 1.0);
    assert!(plan.columns.saturating_mul(plan.rows) <= MAX_TILE_PAINTS);
}

/// 无效尺寸不创建无限循环或非法 GPUI 绘制矩形。
#[test]
fn tile_plan_rejects_non_positive_or_non_finite_dimensions() {
    assert_eq!(tile_plan(100.0, 100.0, 0.0, 10.0), None);
    assert_eq!(tile_plan(f32::INFINITY, 100.0, 10.0, 10.0), None);
}

/// 动画平铺使用一个共享帧时钟，超时前不重绘，超时后按帧间隔推进并循环。
#[test]
fn tile_animation_advances_and_wraps_at_frame_delay() {
    let path = PathBuf::from("C:/Pictures/animated.gif");
    let start = Instant::now();
    let mut state = TileAnimationState::default();

    let first = advance_tile_animation(
        &mut state,
        &path,
        2,
        Duration::from_millis(100),
        start,
        true,
        false,
    );
    let second = advance_tile_animation(
        &mut state,
        &path,
        2,
        Duration::from_millis(100),
        start + Duration::from_millis(99),
        true,
        false,
    );
    let third = advance_tile_animation(
        &mut state,
        &path,
        2,
        Duration::from_millis(100),
        start + Duration::from_millis(100),
        true,
        false,
    );
    let fourth = advance_tile_animation(
        &mut state,
        &path,
        2,
        Duration::from_millis(100),
        start + Duration::from_millis(200),
        true,
        false,
    );

    assert_eq!(first.frame_index, 0);
    assert_eq!(second.frame_index, 0);
    assert_eq!(third.frame_index, 1);
    assert_eq!(fourth.frame_index, 0);
    assert!(first.request_frame && third.request_frame);
}

/// 非活动窗口和系统减弱动态效果时暂停动画；恢复后的首帧重置时钟，完整等待一帧间隔再推进。
#[test]
fn tile_animation_pauses_without_losing_frame_position() {
    let path = PathBuf::from("C:/Pictures/animated.webp");
    let start = Instant::now();
    let mut state = TileAnimationState::default();
    advance_tile_animation(
        &mut state,
        &path,
        3,
        Duration::from_millis(50),
        start,
        true,
        false,
    );
    let paused = advance_tile_animation(
        &mut state,
        &path,
        3,
        Duration::from_millis(50),
        start + Duration::from_millis(250),
        false,
        false,
    );
    let resumed = advance_tile_animation(
        &mut state,
        &path,
        3,
        Duration::from_millis(50),
        start + Duration::from_millis(500),
        true,
        true,
    );
    let active_after_resume = advance_tile_animation(
        &mut state,
        &path,
        3,
        Duration::from_millis(50),
        start + Duration::from_millis(550),
        true,
        false,
    );
    let scheduled = advance_tile_animation(
        &mut state,
        &path,
        3,
        Duration::from_millis(50),
        start + Duration::from_millis(600),
        true,
        false,
    );

    assert_eq!(paused.frame_index, 0);
    assert_eq!(resumed.frame_index, 0);
    assert!(!paused.request_frame && !resumed.request_frame);
    assert_eq!(active_after_resume.frame_index, 0);
    assert!(active_after_resume.request_frame);
    assert_eq!(scheduled.frame_index, 1);
    assert!(scheduled.request_frame);
}

/// 切换动画文件时重置帧索引，静态或空图不申请动画帧调度。
#[test]
fn tile_animation_resets_for_new_resource_and_skips_static_images() {
    let first_path = PathBuf::from("C:/Pictures/first.gif");
    let next_path = PathBuf::from("C:/Pictures/second.gif");
    let start = Instant::now();
    let mut state = TileAnimationState::default();
    advance_tile_animation(
        &mut state,
        &first_path,
        4,
        Duration::from_millis(10),
        start,
        true,
        false,
    );
    advance_tile_animation(
        &mut state,
        &first_path,
        4,
        Duration::from_millis(10),
        start + Duration::from_millis(10),
        true,
        false,
    );

    let switched = advance_tile_animation(
        &mut state,
        &next_path,
        1,
        Duration::from_millis(10),
        start + Duration::from_millis(20),
        true,
        false,
    );
    let empty = advance_tile_animation(
        &mut state,
        &next_path,
        0,
        Duration::from_millis(10),
        start + Duration::from_millis(30),
        true,
        false,
    );

    assert_eq!(switched.frame_index, 0);
    assert!(!switched.request_frame);
    assert_eq!(empty.frame_index, 0);
    assert!(!empty.request_frame);
}

/// 固定图片路径与平铺偏好，其余沿用默认值；数据投影不应在 UI 初始化时探测文件系统。
///
/// @author kongweiguang
#[test]
fn settings_projection_preserves_selected_path_without_io() {
    let snapshot = SettingsSnapshot {
        background_enabled: true,
        background_image_path: "C:/Pictures/terminal.png".to_owned(),
        background_fit: BackgroundFit::Tile,
        ..SettingsSnapshot::default()
    };
    let projected = TerminalBackgroundSettings::from_snapshot(&snapshot);

    assert_eq!(
        projected.image_path,
        Some(PathBuf::from("C:/Pictures/terminal.png"))
    );
    assert_eq!(projected.fit, BackgroundFit::Tile);
}
