//! 真实 Root 搜索浮层 host 的布局回归，确认浮层显隐不改变 Canvas 网格。
//!
//! @author kongweiguang

use super::{OVERLAY_MAX_WIDTH, terminal_search_overlay_element};
use crate::{
    features::terminal_tools::TerminalToolsView, terminal_canvas::terminal_size_for_canvas,
    terminal_metrics::TerminalMetrics,
};
use gpui_kit::{
    AppContext as _, Bounds, Context, Entity, IntoElement, Pixels, Render, TestAppContext,
    VisualTestContext, Window, div, prelude::*, px, size,
};
use kerminal_application::preferences::{TerminalSettings, ThemeMode};
use kerminal_domain::{identity::SessionId, sessions::TerminalSize};
use std::{cell::RefCell, rc::Rc};

struct SearchOverlayHarness {
    tools: Entity<TerminalToolsView>,
    metrics: TerminalMetrics,
}

impl Render for SearchOverlayHarness {
    /// 测试复用 Root 同一个绝对定位浮层 builder，只用可测量 Canvas 固定宿主尺寸。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut host = div()
            .id("terminal-search-test-canvas-host")
            .size_full()
            .relative()
            .child(
                div()
                    .id("terminal-search-test-canvas")
                    .debug_selector(|| "terminal-search-test-canvas".to_owned())
                    .size_full(),
            );
        if let Some(search) = self.tools.read(cx).search_overlay() {
            let placement = search.read(cx).overlay_placement();
            host = host.child(terminal_search_overlay_element(
                search,
                placement,
                &self.metrics,
            ));
        }
        host
    }
}

/// 初始化真实 TerminalToolsView 与 Root 浮层 host，使测试覆盖实际 GPUI 布局而非宽高公式副本。
///
/// @author kongweiguang
fn fixture(
    cx: &mut TestAppContext,
) -> (
    Entity<SearchOverlayHarness>,
    SessionId,
    &mut VisualTestContext,
) {
    cx.update(|app| {
        gpui_kit::init(app);
        app.set_reduce_motion(true);
    });
    let session_id = SessionId::new();
    let tool_session_id = session_id.clone();
    let returned = Rc::new(RefCell::new(None));
    let output = returned.clone();
    let (_, visual) = cx.add_window_view(move |window, cx| {
        let tools = cx.new(|cx| TerminalToolsView::new(Some(tool_session_id.clone()), window, cx));
        let harness = cx.new(|_| SearchOverlayHarness {
            tools,
            metrics: TerminalMetrics::from_settings(
                &TerminalSettings::default(),
                &["Consolas".to_owned()],
            ),
        });
        *output.borrow_mut() = Some(harness.clone());
        gpui_kit::base::Root::new(harness, window, cx)
    });
    let harness = returned
        .borrow_mut()
        .take()
        .expect("search overlay harness mounted");
    draw(visual);
    (harness, session_id, visual)
}

/// 刷新异步实体变更并绘制两帧，等待真实浮层测量值回写到稳定的搜索实体。
///
/// @author kongweiguang
fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, app| window.draw(app).clear(app));
    cx.run_until_parked();
    cx.update(|window, app| window.draw(app).clear(app));
}

/// 打开同一 session 的生产搜索控件，Root host 后续直接读取其稳定 Toolbar Entity。
///
/// @author kongweiguang
fn open_search(
    harness: &Entity<SearchOverlayHarness>,
    session_id: &SessionId,
    cx: &mut VisualTestContext,
) {
    cx.update(|window, app| {
        harness.update(app, |harness, cx| {
            harness.tools.update(cx, |tools, cx| {
                assert!(tools.open_search_for(session_id, window, cx));
            });
        });
    });
    draw(cx);
}

/// 关闭搜索但保留工具 Entity，覆盖用户关闭后同 pane 再次打开的布局路径。
///
/// @author kongweiguang
fn close_search(harness: &Entity<SearchOverlayHarness>, cx: &mut VisualTestContext) {
    cx.update(|window, app| {
        harness.update(app, |harness, cx| {
            harness
                .tools
                .update(cx, |tools, cx| tools.close_search(window, cx));
        });
    });
    draw(cx);
}

/// 用终端 Canvas 正式尺寸函数从本帧实际 bounds 推导 PTY 网格，作为 resize 请求门禁输入。
///
/// @author kongweiguang
fn canvas_geometry(
    cx: &mut VisualTestContext,
    harness: &Entity<SearchOverlayHarness>,
) -> (Bounds<Pixels>, Option<TerminalSize>) {
    let bounds = cx
        .debug_bounds("terminal-search-test-canvas")
        .expect("test Canvas has measured bounds");
    let size = cx.update(|window, app| {
        harness.read_with(app, |harness, _| {
            let metrics = &harness.metrics;
            terminal_size_for_canvas(
                bounds,
                metrics,
                metrics.cell_width(window),
                window.scale_factor(),
            )
        })
    });
    (bounds, size)
}

/// 对照终端当前快照尺寸应用真实 Canvas resize 条件，统计 overlay 是否提出新尺寸请求。
///
/// @author kongweiguang
fn resize_request_count(measured: Option<TerminalSize>, snapshot_size: TerminalSize) -> usize {
    if measured.is_some_and(|size| size != snapshot_size) {
        1
    } else {
        0
    }
}

/// 真实共同 host 下，关闭、打开、错误扩展和窄 pane 都不改变 Canvas bounds 或网格尺寸。
///
/// 计数采用 terminal_canvas 的 `snapshot.size != terminal_size_for_canvas(...)` 提交条件；只有宿主
/// 尺寸本身变化时才把窄 pane 的测量尺寸作为新的快照基准。
///
/// @author kongweiguang
#[gpui_kit::test]
fn floating_search_keeps_canvas_bounds_and_resize_requests_stable(app: &mut TestAppContext) {
    let (harness, session_id, visual) = fixture(app);
    visual.simulate_resize(size(px(900.), px(640.)));
    draw(visual);

    let (closed_bounds, closed_size) = canvas_geometry(visual, &harness);
    let snapshot_size = closed_size.expect("visible test Canvas maps to a terminal grid");

    open_search(&harness, &session_id, visual);
    let (open_bounds, open_size) = canvas_geometry(visual, &harness);
    let card_bounds = visual
        .debug_bounds("terminal-search-overlay-card-bounds")
        .expect("visible search card has actual bounds");
    assert_eq!(open_bounds, closed_bounds);
    assert_eq!(open_size, closed_size);
    assert_eq!(card_bounds.size.width, px(OVERLAY_MAX_WIDTH));
    assert!((card_bounds.size.height.as_f32() - 44.0).abs() <= 2.0);
    assert_eq!(card_bounds.origin.y, closed_bounds.origin.y + px(8.));
    assert_eq!(
        card_bounds.origin.x + card_bounds.size.width,
        closed_bounds.origin.x + closed_bounds.size.width - px(8.),
    );
    assert_eq!(resize_request_count(open_size, snapshot_size), 0);

    let toolbar = visual.update(|_, app| {
        harness.read_with(app, |harness, app| {
            harness
                .tools
                .read(app)
                .search_overlay()
                .expect("search remains open")
        })
    });
    visual.update(|_, app| {
        toolbar.update(app, |toolbar, cx| {
            toolbar.set_error(&session_id, 0, "invalid regular expression", cx);
        });
    });
    draw(visual);
    let (error_bounds, error_size) = canvas_geometry(visual, &harness);
    let error_card_bounds = visual
        .debug_bounds("terminal-search-overlay-card-bounds")
        .expect("error feedback stays inside the floating card");
    assert_eq!(error_bounds, closed_bounds);
    assert_eq!(error_size, closed_size);
    assert!(error_card_bounds.size.height > card_bounds.size.height);
    assert_eq!(resize_request_count(error_size, snapshot_size), 0);

    close_search(&harness, visual);
    let (reclosed_bounds, reclosed_size) = canvas_geometry(visual, &harness);
    assert_eq!(reclosed_bounds, closed_bounds);
    assert_eq!(reclosed_size, closed_size);

    visual.simulate_resize(size(px(360.), px(640.)));
    draw(visual);
    let (narrow_closed_bounds, narrow_closed_size) = canvas_geometry(visual, &harness);
    let narrow_snapshot_size =
        narrow_closed_size.expect("narrow Canvas remains a valid terminal grid");

    open_search(&harness, &session_id, visual);
    let (narrow_open_bounds, narrow_open_size) = canvas_geometry(visual, &harness);
    let narrow_card = visual
        .debug_bounds("terminal-search-overlay-card-bounds")
        .expect("narrow search card has actual bounds");
    assert_eq!(narrow_open_bounds, narrow_closed_bounds);
    assert_eq!(narrow_open_size, narrow_closed_size);
    assert_eq!(narrow_card.size.width, px(344.));
    assert!(narrow_card.size.width < px(400.));
    assert_eq!(narrow_card.origin.y, narrow_closed_bounds.origin.y + px(8.));
    assert_eq!(
        narrow_card.origin.x + narrow_card.size.width,
        narrow_closed_bounds.origin.x + narrow_closed_bounds.size.width - px(8.),
    );
    assert_eq!(
        resize_request_count(narrow_open_size, narrow_snapshot_size),
        0
    );

    close_search(&harness, visual);
}

/// 用 GPUI 的显示缩放事件覆盖像素尺寸换算，并使同一浮层实体经历浅色、深色和系统色板切换。
/// 这里验证布局与度量；真实 Windows 显示器 DPI 和肉眼可读性仍需独立实窗证据。
///
/// @author kongweiguang
#[gpui_kit::test]
fn floating_search_preserves_geometry_across_themes_and_display_scales(app: &mut TestAppContext) {
    let (harness, session_id, visual) = fixture(app);
    visual.simulate_resize(size(px(900.), px(640.)));
    for scale in [1.0, 1.5, 2.0] {
        visual.simulate_scale_factor_change(scale);
        for mode in [ThemeMode::Light, ThemeMode::Dark, ThemeMode::System] {
            visual.update(|window, app| {
                harness.update(app, |_, cx| {
                    crate::theme::apply_theme_mode(mode, window.appearance(), cx);
                });
            });
            draw(visual);
            let closed = canvas_geometry(visual, &harness);
            open_search(&harness, &session_id, visual);
            assert_eq!(canvas_geometry(visual, &harness), closed);
            let card = visual
                .debug_bounds("terminal-search-overlay-card-bounds")
                .expect("themed card is mounted");
            assert_eq!(card.size.width, px(440.));
            close_search(&harness, visual);
            assert_eq!(canvas_geometry(visual, &harness), closed);
        }
    }
}

/// 安全行按卡片高度推导，与当前锚点无关；异常度量拒绝生成可用于定位的边距。
///
/// @author kongweiguang
#[test]
fn measured_safe_rows_follow_card_height() {
    assert_eq!(super::measured_safe_top_rows(44.0, 20.0, 12.0), 2,);
    assert_eq!(super::measured_safe_top_rows(72.0, 20.0, 12.0), 4,);
    assert_eq!(super::measured_safe_top_rows(f32::NAN, 20.0, 12.0), 0,);
}
