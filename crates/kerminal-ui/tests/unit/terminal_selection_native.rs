//! 选区原生视觉探针：在隐藏窗口中用生产几何与文字颜色绘制样张，读取真实 renderer 像素。
//!
//! 探针不创建终端会话、不读取用户配置、不显示或聚焦窗口；PNG 只作为人工复核材料，
//! 不把截图等同于审美验收或真实终端功能验收。该入口需要可见/私有桌面 runner：隐藏窗口
//! 在本机可能一直返回 1×1 render target，失败时 report 记录真实尺寸且不写占位图。
//!
//! @author kongweiguang

use std::{
    env, fs, io,
    path::{Path as FilePath, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use gpui_kit::{
    AnyWindowHandle, App, AsyncApp, Bounds, Context, Entity, Hsla, Path, Pixels, Point, Render,
    ShapedLine, TextAlign, TextStyle, Window, WindowBounds, WindowOptions, canvas,
    component::TitleBar, div, fill, point, prelude::*, px, size,
};
use image::{RgbaImage, imageops::FilterType};
use kerminal_application::preferences::{TerminalColorScheme, TerminalSettings, ThemeMode};
use kerminal_terminal::snapshot::{
    TerminalAttributes, TerminalColor, TerminalPalette, TerminalRgb, TerminalStyle,
};
use serde::Serialize;

use super::overlay::{CellOverlayPaint, TerminalOverlayKind};
use super::selection_geometry::{
    SelectionRowSpan, outline_vertices, selection_bands, selection_paths, selection_regions,
};
use super::shape_cached_line;
use super::style::{
    apply_overlay_paint, apply_terminal_style, effective_terminal_foreground, terminal_rgb,
};
use crate::terminal_metrics::TerminalMetrics;
use crate::theme::{
    TerminalSelectionStyle, terminal_background_color, terminal_palette_for_mode,
    terminal_selection_style,
};

/// 探针样张的固定物理像素尺寸；窗口按逻辑尺寸渲染后统一归一到该尺寸保存。
const OUTPUT_WIDTH: u32 = 900;
const OUTPUT_HEIGHT: u32 = 520;
const FONT_SIZE: f32 = 14.0;
const LINE_COLUMNS: u16 = 120;
const CHECKER_LAST_ROW: u16 = 12;
const SINGLE_ROW: u16 = 4;
const SINGLE_START_COLUMN: u16 = 12;
const SINGLE_END_COLUMN: u16 = 28;
const CONTINUOUS_ROWS: [(u16, u16, u16); 3] = [(6, 0, 14), (7, 0, 32), (8, 0, 20)];
const DISJOINT_ROWS: [(u16, u16, u16); 2] = [(11, 50, 70), (12, 50, 70)];
const CURSOR_ROW: u16 = 14;
const CURSOR_COLUMN: u16 = 80;
const CHECKER_A: u32 = 0xE4572E;
const CHECKER_B: u32 = 0x2E86AB;
/// 渲染器读回值与理论色值允许的抗锯齿/色彩管理误差。
const PIXEL_TOLERANCE: i16 = 4;

/// Canvas 预览状态只保存已准备待绘制的几何和文字，不创建任何终端资源。
struct SelectionNativeHarness {
    palette: TerminalPalette,
    selection: TerminalSelectionStyle,
    metrics: TerminalMetrics,
    facts: Arc<Mutex<SceneFacts>>,
}

impl Render for SelectionNativeHarness {
    /// 每帧按当前终端 palette 重新生成样张，测试 runner 更新状态后可立即截图。
    ///
    /// @author kongweiguang
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        let selection = self.selection;
        let metrics = self.metrics.clone();
        let facts = Arc::clone(&self.facts);
        let preview = canvas(
            move |bounds, window, _cx| {
                build_scene(bounds, window, &metrics, &palette, selection, &facts)
            },
            paint_scene,
        )
        .size_full();
        div().size_full().child(preview)
    }
}

/// 一帧场景的绘制顺序在构建阶段固定，paint 只按该顺序提交，和生产相机一致。
struct SelectionScene {
    background: Hsla,
    checker: Vec<(Bounds<Pixels>, Hsla)>,
    selection_paths: Vec<Path<Pixels>>,
    selection_color: Hsla,
    text: Vec<ShapedText>,
    cursor: Option<(Bounds<Pixels>, Hsla)>,
}

/// 已 shape 的字形和其网格原点；paint 阶段不再重新排版。
struct ShapedText {
    origin: Point<Pixels>,
    line: ShapedLine,
}

/// 供 runner 计算采样点的实际 Canvas 几何，避免测试重复猜测布局。
#[derive(Clone, Copy, Default)]
struct SceneFacts {
    origin_x: f32,
    origin_y: f32,
    width: f32,
    height: f32,
    cell_width: f32,
    line_height: f32,
    inset: f32,
    path_count: usize,
    left_boundary_corners_sharp: bool,
    scale_factor: f32,
    window_active: bool,
}

/// 与生产 `TerminalMetrics::cell_origin` 同源的样张网格，仅用于探针坐标换算。
#[derive(Clone, Copy)]
struct SceneLayout {
    content: Bounds<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
}

impl SceneLayout {
    /// 从真实 Canvas bounds 和终端度量建立内容网格。
    ///
    /// @author kongweiguang
    fn new(bounds: Bounds<Pixels>, cell_width: Pixels, line_height: Pixels, inset: Pixels) -> Self {
        let content = gpui_kit::bounds(
            point(bounds.origin.x + inset, bounds.origin.y + inset),
            size(
                (bounds.size.width - inset * 2.0).max(px(0.0)),
                (bounds.size.height - inset * 2.0).max(px(0.0)),
            ),
        );
        Self {
            content,
            cell_width,
            line_height,
        }
    }

    /// 与终端 Canvas 相同的 cell 原点公式，保证样张文字和选区落在同一网格。
    ///
    /// @author kongweiguang
    fn cell_origin(&self, row: u16, column: u16) -> Point<Pixels> {
        point(
            self.content.origin.x + self.cell_width * usize::from(column),
            self.content.origin.y + self.line_height * usize::from(row),
        )
    }

    /// 返回整格边界；用于棋盘色块，不做裁剪以外的小数缩放。
    ///
    /// @author kongweiguang
    fn cell_bounds(&self, row: u16, column: u16) -> Bounds<Pixels> {
        gpui_kit::bounds(
            self.cell_origin(row, column),
            size(self.cell_width, self.line_height),
        )
    }

    /// 内容区可容纳的整列数，限制棋盘不会绘制到 inset 之外。
    ///
    /// @author kongweiguang
    fn columns(&self) -> u16 {
        (self.content.size.width / self.cell_width)
            .floor()
            .clamp(0.0, f32::from(u16::MAX)) as u16
    }
}

/// 探针使用的选区样张：单行、贴左边界的三行阶梯、以及隔行不相交的第二区域。
///
/// @author kongweiguang
fn sample_spans() -> Vec<SelectionRowSpan> {
    let mut spans = vec![SelectionRowSpan {
        viewport_row: SINGLE_ROW,
        start_column: SINGLE_START_COLUMN,
        end_column: SINGLE_END_COLUMN,
    }];
    spans.extend(
        CONTINUOUS_ROWS
            .iter()
            .map(|(row, start, end)| SelectionRowSpan {
                viewport_row: *row,
                start_column: *start,
                end_column: *end,
            }),
    );
    spans.extend(
        DISJOINT_ROWS
            .iter()
            .map(|(row, start, end)| SelectionRowSpan {
                viewport_row: *row,
                start_column: *start,
                end_column: *end,
            }),
    );
    spans
}

/// 按 2×2 cell 棋盘周期选择模拟图片的颜色，确保未选中区域仍明显可见。
///
/// @author kongweiguang
fn checker_hex(row: u16, column: u16) -> u32 {
    if ((row / 2) + (column / 2)).is_multiple_of(2) {
        CHECKER_A
    } else {
        CHECKER_B
    }
}

/// 构建样张：终端底色 → 棋盘材料 → 生产选区路径 → 生产文字色 → 可选 cursor。
///
/// @author kongweiguang
fn build_scene(
    bounds: Bounds<Pixels>,
    window: &mut Window,
    metrics: &TerminalMetrics,
    palette: &TerminalPalette,
    selection: TerminalSelectionStyle,
    facts: &Mutex<SceneFacts>,
) -> SelectionScene {
    let cell_width = metrics.cell_width(window);
    let line_height = px(metrics.line_height);
    let inset = px(metrics.inset);
    let layout = SceneLayout::new(bounds, cell_width, line_height, inset);

    let mut checker = Vec::new();
    for row in 0..=CHECKER_LAST_ROW {
        for column in 0..layout.columns().min(LINE_COLUMNS) {
            checker.push((
                layout.cell_bounds(row, column),
                gpui_kit::rgb(checker_hex(row, column)).into(),
            ));
        }
    }

    let spans = sample_spans();
    let paths = selection_paths(&spans, bounds, cell_width, line_height, inset);
    let text_style = metrics.text_style(window);
    let mut text = Vec::new();
    for spec in sample_text() {
        push_text_cells(
            &mut text,
            window,
            &layout,
            &text_style,
            palette,
            selection,
            spec,
        );
    }

    let cursor = Some((
        layout.cell_bounds(CURSOR_ROW, CURSOR_COLUMN),
        terminal_rgb(palette.cursor),
    ));
    let left_boundary_corners_sharp = left_boundary_corners_sharp(&spans, &layout, line_height);

    *facts
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = SceneFacts {
        origin_x: bounds.origin.x.as_f32(),
        origin_y: bounds.origin.y.as_f32(),
        width: bounds.size.width.as_f32(),
        height: bounds.size.height.as_f32(),
        cell_width: cell_width.as_f32(),
        line_height: line_height.as_f32(),
        inset: inset.as_f32(),
        path_count: paths.len(),
        left_boundary_corners_sharp,
        scale_factor: window.scale_factor(),
        window_active: window.is_window_active(),
    };

    SelectionScene {
        background: terminal_background_color(*palette),
        checker,
        selection_paths: paths,
        selection_color: terminal_rgb(selection.background),
        text,
        cursor,
    }
}

/// 一行样张文字的描述；选中与否决定是否走生产 Selection overlay 的颜色分支。
struct TextSpec {
    row: u16,
    column: u16,
    text: &'static str,
    color: TerminalColor,
    selected: bool,
}

/// 覆盖默认色、ANSI 红/蓝、选区内的空白 cell 与低对比 ANSI 色。
///
/// @author kongweiguang
fn sample_text() -> Vec<TextSpec> {
    vec![
        TextSpec {
            row: 3,
            column: 4,
            text: "default",
            color: TerminalColor::DefaultForeground,
            selected: false,
        },
        TextSpec {
            row: 3,
            column: 14,
            text: "red",
            color: TerminalColor::Ansi(1),
            selected: false,
        },
        TextSpec {
            row: 3,
            column: 20,
            text: "blue",
            color: TerminalColor::Ansi(4),
            selected: false,
        },
        TextSpec {
            row: SINGLE_ROW,
            column: 16,
            text: "selected",
            color: TerminalColor::DefaultForeground,
            selected: true,
        },
        TextSpec {
            row: SINGLE_ROW,
            column: 25,
            text: "low",
            color: TerminalColor::Ansi(8),
            selected: true,
        },
        TextSpec {
            row: 6,
            column: 1,
            text: "stair",
            color: TerminalColor::DefaultForeground,
            selected: true,
        },
        TextSpec {
            row: 7,
            column: 1,
            text: "wide middle row",
            color: TerminalColor::DefaultForeground,
            selected: true,
        },
        TextSpec {
            row: 8,
            column: 1,
            text: "narrow bottom",
            color: TerminalColor::DefaultForeground,
            selected: true,
        },
        TextSpec {
            row: 11,
            column: 51,
            text: "disjoint",
            color: TerminalColor::DefaultForeground,
            selected: true,
        },
        TextSpec {
            row: 12,
            column: 51,
            text: "region",
            color: TerminalColor::DefaultForeground,
            selected: true,
        },
        TextSpec {
            row: 15,
            column: 8,
            text: "terminal background",
            color: TerminalColor::DefaultForeground,
            selected: false,
        },
    ]
}

/// 逐 cell shape 样张文字，真实复用生产样式与选区文字色，不手写颜色常量。
///
/// @author kongweiguang
fn push_text_cells(
    scene: &mut Vec<ShapedText>,
    window: &mut Window,
    layout: &SceneLayout,
    text_style: &TextStyle,
    palette: &TerminalPalette,
    selection: TerminalSelectionStyle,
    spec: TextSpec,
) {
    let terminal_style = TerminalStyle {
        foreground: spec.color,
        background: TerminalColor::DefaultBackground,
        underline_color: None,
        attributes: TerminalAttributes::default(),
    };
    for (index, character) in spec.text.chars().enumerate() {
        let mut buffer = [0_u8; 4];
        let glyph = character.encode_utf8(&mut buffer);
        let mut run = text_style.to_run(glyph.len());
        apply_terminal_style(&mut run, terminal_style, palette);
        if spec.selected {
            let effective_foreground = effective_terminal_foreground(terminal_style, palette);
            apply_overlay_paint(
                &mut run,
                CellOverlayPaint {
                    kind: TerminalOverlayKind::Selection,
                    style: None,
                },
                palette,
                effective_foreground,
                selection,
            );
        }
        let line = shape_cached_line(
            window,
            glyph,
            px(FONT_SIZE),
            &[run],
            Some(layout.cell_width),
        );
        scene.push(ShapedText {
            origin: layout.cell_origin(spec.row, spec.column + index as u16),
            line,
        });
    }
}

/// 验证贴左侧内容边界的三行选区在左边界上的顶点全部保持直角。
///
/// @author kongweiguang
fn left_boundary_corners_sharp(
    spans: &[SelectionRowSpan],
    layout: &SceneLayout,
    line_height: Pixels,
) -> bool {
    let content_left = layout.content.origin.x.as_f32();
    let content_top = layout.content.origin.y.as_f32();
    let content_right = content_left + layout.content.size.width.as_f32();
    let content_bottom = content_top + layout.content.size.height.as_f32();
    let cell_width = layout.cell_width.as_f32();
    let line_height = line_height.as_f32();

    let mut saw_left_edge = false;
    for region in selection_regions(spans, content_left, cell_width, content_right) {
        let bands = selection_bands(
            &region,
            content_left,
            content_right,
            content_top,
            content_bottom,
            cell_width,
            line_height,
        );
        for vertex in outline_vertices(
            &bands,
            content_left,
            content_right,
            content_top,
            content_bottom,
        ) {
            if (vertex.x - content_left).abs() <= 0.01 {
                saw_left_edge = true;
                if !vertex.on_boundary {
                    return false;
                }
            }
        }
    }
    saw_left_edge
}

/// 按生产顺序提交样张：底色、棋盘、选中轮廓、文字、cursor。
///
/// @author kongweiguang
fn paint_scene(bounds: Bounds<Pixels>, scene: SelectionScene, window: &mut Window, cx: &mut App) {
    let SelectionScene {
        background,
        checker,
        selection_paths,
        selection_color,
        text,
        cursor,
    } = scene;
    window.paint_quad(fill(bounds, background));
    for (cell, color) in checker {
        window.paint_quad(fill(cell, color));
    }
    for path in selection_paths {
        window.paint_path(path, selection_color);
    }
    for run in text {
        let _ = run
            .line
            .paint(run.origin, px(FONT_SIZE), TextAlign::Left, None, window, cx);
    }
    if let Some((cursor, color)) = cursor {
        window.paint_quad(fill(cursor, color));
    }
}

#[derive(Clone, Serialize)]
struct SampleReport {
    name: &'static str,
    logical: [f32; 2],
    actual_rgba: [u8; 4],
    expected_rgba: Option<[u8; 4]>,
    expectation: &'static str,
    passed: bool,
}

#[derive(Clone, Serialize)]
struct ImageReport {
    scheme: &'static str,
    mode: &'static str,
    file: String,
    render_size: [u32; 2],
    image_size: [u32; 2],
    scale: [f32; 2],
    window_active: bool,
    physical_scale: f32,
    palette_background: [u8; 4],
    selection_background: [u8; 4],
    foreground_fallback: [u8; 4],
    path_count: usize,
    left_boundary_corners_sharp: bool,
    samples: Vec<SampleReport>,
}

impl ImageReport {
    /// 汇总几何和像素失败，回报包含实际值以便区分渲染问题与采样问题。
    ///
    /// @author kongweiguang
    fn failure_summary(&self) -> Option<String> {
        let mut issues = Vec::new();
        if !self.left_boundary_corners_sharp {
            issues.push("left content-boundary corners are not sharp".to_owned());
        }
        if self.path_count < 3 {
            issues.push(format!(
                "expected at least 3 selection paths, got {}",
                self.path_count
            ));
        }
        for sample in &self.samples {
            if !sample.passed {
                issues.push(format!(
                    "{}: actual {:?}, expected {:?} ({})",
                    sample.name, sample.actual_rgba, sample.expected_rgba, sample.expectation
                ));
            }
        }
        (!issues.is_empty()).then(|| format!("{}: {}", self.file, issues.join("; ")))
    }
}

/// 显式 ignored 入口只由视觉验收 runner 调用，普通测试不创建原生窗口。
///
/// @author kongweiguang
#[test]
#[ignore = "requires the isolated Windows native renderer and KERMINAL_TERMINAL_SELECTION_OUTPUT"]
fn terminal_selection_native_acceptance() {
    let output = env::var_os("KERMINAL_TERMINAL_SELECTION_OUTPUT")
        .map(PathBuf::from)
        .expect("runner must set KERMINAL_TERMINAL_SELECTION_OUTPUT");
    fs::create_dir_all(&output).expect("create terminal selection output directory");
    let report_path = output.join("report.json");
    if report_path.exists() {
        fs::remove_file(&report_path).expect("remove stale terminal selection report");
    }

    let failure = Arc::new(Mutex::new(None::<String>));
    let reports = Arc::new(Mutex::new(Vec::<ImageReport>::new()));
    let failure_for_app = Arc::clone(&failure);
    let reports_for_app = Arc::clone(&reports);
    let output_for_app = output.clone();

    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            crate::init(cx);
            let window_options = WindowOptions {
                // 隐藏窗口不会收到 WM_PAINT，渲染目标必须在创建时就具备最终尺寸，
                // 否则会一直停留在平台默认的 1×1。
                window_bounds: Some(WindowBounds::Windowed(gpui_kit::bounds(
                    point(px(0.0), px(0.0)),
                    size(px(900.0), px(520.0)),
                ))),
                window_min_size: Some(size(px(900.0), px(520.0))),
                show: false,
                focus: false,
                ..TitleBar::window_options()
            };
            let result = gpui_kit::open_window(window_options, cx, |window, cx| {
                let installed = window.text_system().all_font_names();
                let settings = TerminalSettings {
                    font_size: FONT_SIZE as u16,
                    line_height: f64::from(20.0 / FONT_SIZE),
                    ..TerminalSettings::default()
                };
                let metrics = TerminalMetrics::from_settings(&settings, &installed);
                let palette = terminal_palette_for_mode(
                    window.appearance(),
                    ThemeMode::Light,
                    TerminalColorScheme::Kerminal,
                );
                let harness = cx.new(|_| SelectionNativeHarness {
                    selection: terminal_selection_style(&palette),
                    palette,
                    metrics: metrics.clone(),
                    facts: Arc::new(Mutex::new(SceneFacts::default())),
                });
                let window_handle = window.window_handle();
                let task_failure = Arc::clone(&failure_for_app);
                let task_reports = Arc::clone(&reports_for_app);
                let output = output_for_app.clone();
                let task_harness = harness.clone();
                cx.spawn(async move |cx| {
                    let result =
                        run_native_selection(cx, window_handle, task_harness, output, task_reports)
                            .await;
                    if let Err(error) = result {
                        record_failure(&task_failure, error.to_string());
                    }
                    let _ = window_handle.update(cx, |_, window, _| window.remove_window());
                    cx.update(|cx| cx.quit());
                })
                .detach();
                harness
            });
            if let Err(error) = result {
                record_failure(&failure_for_app, error.to_string());
                cx.quit();
            }
        });

    let error = failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    let reports = reports
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let report = serde_json::json!({
        "status": if error.is_none() { "passed" } else { "failed" },
        "error": error,
        "images": reports,
    });
    fs::write(
        &report_path,
        serde_json::to_vec_pretty(&report).expect("serialize terminal selection report"),
    )
    .expect("write terminal selection report");

    if let Some(error) = error {
        panic!("terminal selection native acceptance failed: {error}");
    }
    assert_eq!(reports.len(), 8, "one image for each scheme and mode");
}

/// 串行覆盖四套终端配色与浅深外观，每张图都基于同一隐藏窗口的真实场景。
///
/// @author kongweiguang
async fn run_native_selection(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    harness: Entity<SelectionNativeHarness>,
    output: PathBuf,
    reports: Arc<Mutex<Vec<ImageReport>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    window.update(cx, |_, window, app| {
        window.resize(size(px(900.0), px(520.0)));
        // 先绘制并模拟两帧，让平台处理 WM_SIZE 后再读取渲染目标。
        draw_simulated_frames(window, app);
        window.refresh();
    })?;
    cx.background_executor()
        .timer(Duration::from_millis(200))
        .await;
    window.update(cx, |_, window, app| draw_simulated_frames(window, app))?;
    cx.background_executor()
        .timer(Duration::from_millis(80))
        .await;

    let schemes = [
        TerminalColorScheme::Kerminal,
        TerminalColorScheme::TokyoNight,
        TerminalColorScheme::Solarized,
        TerminalColorScheme::Github,
    ];
    for scheme in schemes {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            window.update(cx, |_, window, app| {
                let palette = terminal_palette_for_mode(window.appearance(), mode, scheme);
                harness.update(app, |harness, cx| {
                    harness.palette = palette;
                    harness.selection = terminal_selection_style(&palette);
                    cx.notify();
                });
                draw_simulated_frames(window, app);
            })?;
            // 平台在下一帧消费 resize 请求；等待后再次绘制，截图才对应目标尺寸。
            cx.background_executor()
                .timer(Duration::from_millis(60))
                .await;
            let report = window.update(cx, |_, window, app| {
                let palette = terminal_palette_for_mode(window.appearance(), mode, scheme);
                let selection = terminal_selection_style(&palette);
                draw_simulated_frames(window, app);
                let image = window.render_to_image().map_err(io::Error::other)?;
                let facts = *harness
                    .read(app)
                    .facts
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                render_report(&image, facts, &palette, selection, scheme, mode, &output)
                    .map_err(io::Error::other)
            })??;
            if let Some(summary) = report.failure_summary() {
                eprintln!("{summary}");
                reports
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(report);
                return Err(io::Error::other(summary).into());
            }
            reports
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(report);
            cx.background_executor()
                .timer(Duration::from_millis(40))
                .await;
        }
    }
    Ok(())
}

/// 生成 PNG、采样真实像素并记录期望值；任何采样失败返回可读的完整差异。
///
/// @author kongweiguang
fn render_report(
    image: &RgbaImage,
    facts: SceneFacts,
    palette: &TerminalPalette,
    selection: TerminalSelectionStyle,
    scheme: TerminalColorScheme,
    mode: ThemeMode,
    output: &FilePath,
) -> Result<ImageReport, String> {
    if image.width() == 0 || image.height() == 0 {
        return Err("native renderer returned a zero-sized image".to_owned());
    }
    if !image.pixels().any(|pixel| pixel[3] != 0) {
        return Err("native renderer returned a fully transparent image".to_owned());
    }
    if facts.width <= 0.0 || facts.height <= 0.0 || facts.cell_width <= 0.0 {
        return Err("scene facts missing or invalid".to_owned());
    }
    // 隐藏窗口若没有收到 paint，渲染目标会停在 1×1；此时保存得到的只是拉伸色块，
    // 必须直接失败而不是生成看似有效的占位 PNG。
    let logical_aspect = facts.width / facts.height;
    let image_aspect = image.width() as f32 / image.height() as f32;
    if image.width() < 2 || image.height() < 2 || (image_aspect - logical_aspect).abs() > 0.01 {
        return Err(format!(
            "native renderer returned {}x{} for a {}x{} logical canvas (scale {:.3}, window_active={}); hidden window did not receive a paint/resize",
            image.width(),
            image.height(),
            facts.width,
            facts.height,
            facts.scale_factor,
            facts.window_active
        ));
    }

    let file = format!("{}-{}.png", scheme_name(scheme), mode_name(mode));
    save_normalized(image, &output.join(&file))?;

    let scale_x = image.width() as f32 / facts.width;
    let scale_y = image.height() as f32 / facts.height;
    let content_left = facts.origin_x + facts.inset;
    let content_top = facts.origin_y + facts.inset;
    let single_x0 = content_left + facts.cell_width * f32::from(SINGLE_START_COLUMN);
    let single_y0 = content_top + facts.line_height * f32::from(SINGLE_ROW);

    let selection_rgba = rgb_bytes(selection.background);
    let background_rgba = rgb_bytes(palette.background);
    let checker_rgba = hex_bytes(checker_hex(2, 60));

    let mut samples = Vec::new();
    let selected_blank = (
        content_left + facts.cell_width * (f32::from(SINGLE_START_COLUMN) + 2.5),
        content_top + facts.line_height * (f32::from(SINGLE_ROW) + 0.5),
    );
    let checker = (
        content_left + facts.cell_width * 60.5,
        content_top + facts.line_height * 2.5,
    );
    let corner_outside = (single_x0 - 0.2, single_y0 - 0.2);
    let corner_inside = (single_x0 + 4.0, single_y0 + 4.0);
    let terminal_blank = (
        content_left + facts.cell_width * 80.5,
        content_top + facts.line_height * 15.5,
    );

    let actual = sample_pixel(image, selected_blank, scale_x, scale_y);
    samples.push(sample(
        "selected_blank_center",
        selected_blank,
        actual,
        Some(selection_rgba),
        "equals selection background",
        close_rgba(actual, selection_rgba),
    ));

    let actual = sample_pixel(image, checker, scale_x, scale_y);
    samples.push(sample(
        "unselected_checker",
        checker,
        actual,
        Some(checker_rgba),
        "differs from selection background",
        !close_rgba(actual, selection_rgba),
    ));

    let actual = sample_pixel(image, corner_outside, scale_x, scale_y);
    samples.push(sample(
        "rounded_corner_outside",
        corner_outside,
        actual,
        None,
        "outside the selection corner",
        !close_rgba(actual, selection_rgba),
    ));

    let actual = sample_pixel(image, corner_inside, scale_x, scale_y);
    samples.push(sample(
        "rounded_corner_inside",
        corner_inside,
        actual,
        Some(selection_rgba),
        "equals selection background at 4px inward",
        close_rgba(actual, selection_rgba),
    ));

    let actual = sample_pixel(image, terminal_blank, scale_x, scale_y);
    samples.push(sample(
        "unselected_terminal_blank",
        terminal_blank,
        actual,
        Some(background_rgba),
        "equals terminal background",
        close_rgba(actual, background_rgba),
    ));

    Ok(ImageReport {
        scheme: scheme_name(scheme),
        mode: mode_name(mode),
        file,
        render_size: [image.width(), image.height()],
        image_size: [OUTPUT_WIDTH, OUTPUT_HEIGHT],
        scale: [scale_x, scale_y],
        window_active: facts.window_active,
        physical_scale: facts.scale_factor,
        palette_background: background_rgba,
        selection_background: selection_rgba,
        foreground_fallback: rgb_bytes(selection.foreground_fallback),
        path_count: facts.path_count,
        left_boundary_corners_sharp: facts.left_boundary_corners_sharp,
        samples,
    })
}

/// 像素样本记录实际与期望值；失败信息由 ImageReport 汇总后统一打印。
///
/// @author kongweiguang
fn sample(
    name: &'static str,
    logical: (f32, f32),
    actual_rgba: [u8; 4],
    expected_rgba: Option<[u8; 4]>,
    expectation: &'static str,
    passed: bool,
) -> SampleReport {
    SampleReport {
        name,
        logical: [logical.0, logical.1],
        actual_rgba,
        expected_rgba,
        expectation,
        passed,
    }
}

/// 从真实 RGBA 图像按逻辑坐标读取像素，支持非 1.0 DPI 缩放。
///
/// @author kongweiguang
fn sample_pixel(image: &RgbaImage, logical: (f32, f32), scale_x: f32, scale_y: f32) -> [u8; 4] {
    let x = ((logical.0 * scale_x).floor() as i64)
        .clamp(0, i64::from(image.width().saturating_sub(1))) as u32;
    let y = ((logical.1 * scale_y).floor() as i64)
        .clamp(0, i64::from(image.height().saturating_sub(1))) as u32;
    image.get_pixel(x, y).0
}

/// 逐通道比较读回颜色；容差只吸收抗锯齿、DPI 缩放和色彩管理误差。
///
/// @author kongweiguang
fn close_rgba(actual: [u8; 4], expected: [u8; 4]) -> bool {
    actual.iter().zip(expected).all(|(actual, expected)| {
        (i16::from(*actual) - i16::from(expected)).abs() <= PIXEL_TOLERANCE
    })
}

/// 保持 DPI 缩放不影响交付文件尺寸；采样仍读取原始 renderer 图像。
///
/// @author kongweiguang
fn save_normalized(image: &RgbaImage, path: &FilePath) -> Result<(), String> {
    let normalized = if image.width() == OUTPUT_WIDTH && image.height() == OUTPUT_HEIGHT {
        image.clone()
    } else {
        image::imageops::resize(image, OUTPUT_WIDTH, OUTPUT_HEIGHT, FilterType::Triangle)
    };
    normalized
        .save(path)
        .map_err(|error| format!("save {}: {error}", path.display()))
}

/// 同步绘制一帧并模拟两次下一帧回调，给平台机会消费尺寸事件和重建渲染目标。
///
/// @author kongweiguang
fn draw_simulated_frames(window: &mut Window, app: &mut App) {
    draw_frame(window, app);
    window.simulate_next_frame(app);
    draw_frame(window, app);
    window.simulate_next_frame(app);
}

/// 同一帧的布局脏标记只服务测试；清理后返回，避免 arena 在后续断言期间累积。
///
/// @author kongweiguang
fn draw_frame(window: &mut Window, app: &mut App) {
    let clear = window.draw(app);
    clear.clear(app);
}

/// 保留首次失败，后续清理错误不覆盖最初原因。
///
/// @author kongweiguang
fn record_failure(failure: &Mutex<Option<String>>, message: String) {
    let mut slot = failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_none() {
        *slot = Some(message);
    }
}

/// 终端 RGB 是报告与期望像素的稳定来源，避免通过 Hsla 往返引入误差。
///
/// @author kongweiguang
fn rgb_bytes(color: TerminalRgb) -> [u8; 4] {
    [color.red, color.green, color.blue, 0xff]
}

/// 棋盘色块以同一个十六进制来源生成绘制色和报告期望色。
///
/// @author kongweiguang
fn hex_bytes(hex: u32) -> [u8; 4] {
    [
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
        0xff,
    ]
}

/// 文件命名直接体现配色和明暗，便于视觉验收时快速定位。
///
/// @author kongweiguang
fn scheme_name(scheme: TerminalColorScheme) -> &'static str {
    match scheme {
        TerminalColorScheme::Kerminal => "kerminal",
        TerminalColorScheme::TokyoNight => "tokyonight",
        TerminalColorScheme::Solarized => "solarized",
        TerminalColorScheme::Github => "github",
    }
}

/// 文件命名直接体现配色和明暗，便于视觉验收时快速定位。
///
/// @author kongweiguang
fn mode_name(mode: ThemeMode) -> &'static str {
    match mode {
        ThemeMode::Light => "light",
        ThemeMode::Dark => "dark",
        ThemeMode::System => "system",
    }
}
