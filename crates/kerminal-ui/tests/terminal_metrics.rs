//! 终端绘制、鼠标定位和输入法位置共享度量的几何契约。
//!
//! @author kongweiguang

mod features {
    pub mod terminal_tools {
        pub mod selection {
            #[derive(Clone, Copy, Debug, Eq, PartialEq)]
            pub struct ViewportCell {
                pub row: u16,
                pub column: u16,
            }
        }
    }
}

#[path = "../src/terminal_metrics.rs"]
#[allow(
    dead_code,
    reason = "this path-imported geometry test covers window-free APIs; production canvas and IME consume glyph measurement"
)]
mod terminal_metrics;

use gpui_kit::{FontWeight, point, px};
use kerminal_application::preferences::{
    TerminalCursorStyle, TerminalFontWeight, TerminalSettings,
};
use kerminal_terminal::snapshot::TerminalCursorShape;
use terminal_metrics::{TerminalMetrics, effective_cursor_shape, font_weight};

/// 用模拟的 GPUI 字体清单构造度量，让外置测试验证字体选择而不依赖机器安装内容。
///
/// @author kongweiguang
fn test_metrics(settings: &TerminalSettings) -> TerminalMetrics {
    let installed_font_families = [
        "Cascadia Code",
        "Consolas",
        "Courier New",
        "DejaVu Sans Mono",
        "Liberation Mono",
        "Menlo",
        "SF Mono",
        "Ubuntu Mono",
    ]
    .map(str::to_owned);
    TerminalMetrics::from_settings(settings, &installed_font_families)
}

/// 验证 canvas 绘制点与鼠标逆映射使用同一 inset、列宽和行高。
///
/// @author kongweiguang
#[test]
fn cell_origin_and_point_mapping_share_geometry() {
    let metrics = test_metrics(&TerminalSettings::default());
    let element_origin = point(px(20.0), px(40.0));
    let cell_width = px(8.0);
    let cell = metrics.cell_origin(element_origin, 3, 4, cell_width);

    assert_eq!(cell.x, px(20.0 + metrics.inset + 4.0 * 8.0));
    assert_eq!(cell.y, px(40.0 + metrics.inset + 3.0 * metrics.line_height));
    assert_eq!(
        metrics.point_to_cell(element_origin, cell, cell_width),
        Some(features::terminal_tools::selection::ViewportCell { row: 3, column: 4 })
    );
}

/// 拒绝落在终端内边距外的指针，避免边缘点击被误算成首行首列。
///
/// @author kongweiguang
#[test]
fn point_mapping_rejects_inset_and_invalid_cell_width() {
    let metrics = test_metrics(&TerminalSettings::default());
    let origin = point(px(0.0), px(0.0));

    assert_eq!(
        metrics.point_to_cell(origin, point(px(1.0), px(20.0)), px(8.0)),
        None
    );
    assert_eq!(
        metrics.point_to_cell(origin, point(px(20.0), px(20.0)), px(0.0)),
        None
    );
}

/// 确认运行时度量携带动态外观设置，Canvas 不会只保存字号而遗漏字重和光标行为。
///
/// @author kongweiguang
#[test]
fn appearance_metrics_follow_terminal_settings() {
    let settings = TerminalSettings {
        font_weight: TerminalFontWeight::Bold,
        cursor_style: TerminalCursorStyle::Underline,
        cursor_blink: false,
        ..TerminalSettings::default()
    };

    let metrics = test_metrics(&settings);

    assert_eq!(metrics.font_weight, TerminalFontWeight::Bold);
    assert_eq!(font_weight(metrics.font_weight), FontWeight::from(600.0));
    assert_eq!(metrics.cursor_style, TerminalCursorStyle::Underline);
    assert!(!metrics.cursor_blink);
}

/// 按 CSS 候选顺序忽略缺失字体，并保留已安装字体的真实名称与大小写。
///
/// @author kongweiguang
#[test]
fn font_family_uses_first_installed_css_candidate() {
    let settings = TerminalSettings {
        font_family: "Missing Mono, 'sf mono', Consolas, monospace".to_owned(),
        ..TerminalSettings::default()
    };
    let installed_font_families = ["Consolas", "SF Mono"].map(str::to_owned);

    let metrics = TerminalMetrics::from_settings(&settings, &installed_font_families);

    assert_eq!(metrics.font_family.as_ref(), "SF Mono");
}

/// 在配置列表全缺失时，只选 GPUI 实际列出的平台等宽候选。
///
/// @author kongweiguang
#[test]
fn font_family_uses_installed_platform_monospace_fallback() {
    #[cfg(target_os = "windows")]
    let expected = "Cascadia Code";
    #[cfg(target_os = "macos")]
    let expected = "Menlo";
    #[cfg(all(unix, not(target_os = "macos")))]
    let expected = "Ubuntu Mono";
    #[cfg(not(any(target_os = "windows", unix)))]
    let expected = "Cascadia Code";

    let settings = TerminalSettings {
        font_family: "Unavailable Config Font".to_owned(),
        ..TerminalSettings::default()
    };
    let installed_font_families = [expected].map(str::to_owned);

    let metrics = TerminalMetrics::from_settings(&settings, &installed_font_families);

    assert_eq!(metrics.font_family.as_ref(), expected);
}

/// 已安装清单没有受支持的等宽族时，明确保留 generic monospace 而不挑选比例字体。
///
/// @author kongweiguang
#[test]
fn font_family_does_not_substitute_an_unrelated_installed_font() {
    let settings = TerminalSettings {
        font_family: "Unavailable Config Font, monospace".to_owned(),
        ..TerminalSettings::default()
    };
    let installed_font_families = ["Arial"].map(str::to_owned);

    let metrics = TerminalMetrics::from_settings(&settings, &installed_font_families);

    assert_eq!(metrics.font_family.as_ref(), "monospace");
}

/// 用户默认光标只覆盖未被终端显式设置的形态，DECSCUSR 的当前值始终优先。
///
/// @author kongweiguang
#[test]
fn configured_cursor_style_yields_to_terminal_style() {
    assert_eq!(
        effective_cursor_shape(
            TerminalCursorStyle::Bar,
            TerminalCursorShape::Underline,
            false,
        ),
        TerminalCursorShape::Beam
    );
    assert_eq!(
        effective_cursor_shape(
            TerminalCursorStyle::Bar,
            TerminalCursorShape::Underline,
            true,
        ),
        TerminalCursorShape::Underline
    );
}
