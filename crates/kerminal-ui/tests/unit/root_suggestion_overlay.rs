//! 光标候选菜单在窗口边缘和窄分屏中的定位回归。
//!
//! @author kongweiguang

use gpui_kit::{
    Bounds, Context, IntoElement, Render, TestAppContext, Window, div, point, prelude::*, px, size,
};
use kerminal_application::preferences::{TerminalFontWeight, TerminalSettings};
use kerminal_domain::{identity::SessionId, sessions::TerminalSize};
use kerminal_terminal::{
    engine::{TerminalEngine, TerminalLimits},
    snapshot::{RenderSnapshot, RenderSnapshotRequest},
};

use super::{
    InlineGhostLine, anchor_available, has_native_prediction, inline_ghost, inline_ghost_geometry,
    menu_position,
};
use crate::terminal_metrics::{TerminalMetrics, font_weight};

/// 只承载生产覆盖层，不复制其排版实现；窗口缩放必须进入真实 GPUI 布局流程。
struct InlineGhostHost {
    metrics: TerminalMetrics,
    cursor: kerminal_terminal::snapshot::TerminalCursor,
}

impl Render for InlineGhostHost {
    /// 用与正文同源的 pane 原点挂载提示，捕获 UI 文本取整导致的亚像素偏移。
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("ghost-pane")
            .debug_selector(|| "ghost-pane".to_owned())
            .size_full()
            .child(inline_ghost(
                "ghost-regression".into(),
                "suggestionfixture中文".into(),
                self.cursor,
                self.metrics.clone(),
                gpui_kit::rgb(0x888888).into(),
            ))
    }
}

/// 小数行高与 pane 原点必须原样进入字形绘制；靠近右下边缘仅裁剪，不可上移或挤回文字。
#[test]
fn inline_ghost_preserves_fractional_origin_and_clips_without_repositioning() {
    let metrics = TerminalMetrics::from_settings(&TerminalSettings::default(), &[]);
    let mut cursor = engine_snapshot(b"git ").cursor;
    let pane = Bounds::new(point(px(3.25), px(7.75)), size(px(230.5), px(180.5)));
    for row in [0, 1, 7] {
        cursor.row = row;
        cursor.column = 24;
        let (origin, clip) = inline_ghost_geometry(pane, cursor, &metrics, px(8.75));
        assert_eq!(origin, metrics.cell_origin(pane.origin, row, 24, px(8.75)));
        assert_eq!(clip.origin, origin);
        assert!(clip.right() <= pane.right());
        assert!(clip.bottom() <= pane.bottom());
    }
}

/// 观测节点允许 UI 像素取整，但真正绘制的行参数必须与正文同源；不能用节点对齐冒充字形对齐。
#[gpui_kit::test]
fn inline_ghost_matches_terminal_metrics_across_fractional_rows_and_scales(
    app: &mut TestAppContext,
) {
    app.update(crate::init);
    let cursor = engine_snapshot(b"git ").cursor;
    let (host, visual) = app.add_window_view(move |_, _| InlineGhostHost {
        metrics: TerminalMetrics::from_settings(&TerminalSettings::default(), &[]),
        cursor,
    });
    visual.simulate_resize(size(px(320.), px(360.)));
    for font_size in [13, 15] {
        for weight in [TerminalFontWeight::Normal, TerminalFontWeight::Bold] {
            for line_height in [1.35, 1.7] {
                let settings = TerminalSettings {
                    font_size,
                    font_weight: weight,
                    line_height,
                    ..TerminalSettings::default()
                };
                for row in [0, 1, 7] {
                    visual.update(|_, cx| {
                        host.update(cx, |host, cx| {
                            host.metrics = TerminalMetrics::from_settings(&settings, &[]);
                            host.cursor.row = row;
                            cx.notify();
                        });
                    });
                    for scale in [1., 1.25, 1.5, 2.] {
                        visual.simulate_scale_factor_change(scale);
                        visual.run_until_parked();
                        visual.update(|window, cx| {
                            window.refresh();
                            window.draw(cx).clear(cx);
                            let pane = window.debug_selector_bounds("ghost-pane").unwrap();
                            let ghost = window.debug_selector_bounds("ghost-regression").unwrap();
                            let host = host.read(cx);
                            let width = host.metrics.cell_width(window);
                            let expected = host.metrics.cell_origin(
                                pane.origin,
                                row,
                                host.cursor.column,
                                width,
                            );
                            assert!((ghost.origin.x - expected.x).abs() <= px(0.5 / scale));
                            assert!((ghost.origin.y - expected.y).abs() <= px(0.5 / scale));
                            let style = host.metrics.text_style(window);
                            assert_eq!(style.font_weight, font_weight(weight));
                            assert_eq!(
                                style.line_height.to_pixels(
                                    px(host.metrics.font_size).into(),
                                    window.rem_size()
                                ),
                                px(host.metrics.line_height),
                            );
                            for text in ["suggestionfixture", "中文👩‍💻"] {
                                let painted = InlineGhostLine::new(
                                    pane,
                                    host.cursor,
                                    &host.metrics,
                                    text.to_owned().into(),
                                    gpui_kit::rgb(0x888888).into(),
                                    window,
                                );
                                let reference = window.text_system().shape_line(
                                    text.to_owned().into(),
                                    px(host.metrics.font_size),
                                    &[style.to_run(text.len())],
                                    text.is_ascii().then_some(width),
                                );
                                assert_eq!(painted.origin, expected);
                                assert_eq!(painted.line_height, px(host.metrics.line_height));
                                assert_eq!(painted.line.ascent, reference.ascent);
                                assert_eq!(painted.line.descent, reference.descent);
                                assert_eq!(painted.line.width(), reference.width());
                                assert!(painted.clip.right() <= pane.right());
                                assert!(painted.clip.bottom() <= pane.bottom());
                            }
                        });
                    }
                }
            }
        }
    }
}

/// 使用真实终端网格生成候选测试快照，避免手写 cell span 误判 Shell 预测。
fn engine_snapshot(output: &[u8]) -> RenderSnapshot {
    let size = TerminalSize::new(80, 24, 0, 0).expect("valid terminal dimensions");
    let mut engine = TerminalEngine::new(SessionId::new(), size, TerminalLimits::default())
        .expect("bounded terminal engine");
    engine.process_output(output);
    engine.render_snapshot(RenderSnapshotRequest::default())
}

/// 下方空间足够时保持光标对齐，避免菜单在普通输入中跳向窗口角落。
#[test]
fn menu_anchors_below_cursor_when_space_is_available() {
    let pane = Bounds::new(point(px(10.), px(20.)), size(px(500.), px(400.)));
    let result = menu_position(
        pane,
        point(px(120.), px(100.)),
        px(20.),
        size(px(200.), px(120.)),
    );
    assert_eq!(result, point(px(120.), px(120.)));
}

/// 靠近右下边缘时向上翻转并横向夹紧；菜单不会溢出当前 pane。
#[test]
fn menu_flips_and_clamps_to_pane() {
    let pane = Bounds::new(point(px(10.), px(20.)), size(px(300.), px(200.)));
    let result = menu_position(
        pane,
        point(px(290.), px(190.)),
        px(20.),
        size(px(200.), px(100.)),
    );
    assert_eq!(result, point(px(110.), px(90.)));
}

/// 只有光标后的可见字符被视为现有 Shell 预测，光标前的命令文本不应造成误判。
#[test]
fn native_prediction_is_detected_after_the_real_cursor() {
    let plain = engine_snapshot(b"git che");
    assert!(!has_native_prediction(&plain));
    let predicted = engine_snapshot(b"git checkout\x1b[5D");
    assert!(has_native_prediction(&predicted));
}

/// 滚动历史、备用屏、隐藏光标或选区时，当前帧不再具备可安全绘制的锚点。
#[test]
fn anchor_rejects_selection_scrollback_alternate_screen_and_hidden_cursor() {
    let snapshot = engine_snapshot(b"git che");
    assert!(anchor_available(&snapshot, false));
    assert!(!anchor_available(&snapshot, true));
    let mut invalid = snapshot.clone();
    invalid.scrollback_offset = 1;
    assert!(!anchor_available(&invalid, false));
    invalid = snapshot.clone();
    invalid.modes.alternate_screen = true;
    assert!(!anchor_available(&invalid, false));
    invalid = snapshot;
    invalid.cursor.visible = false;
    assert!(!anchor_available(&invalid, false));
}
