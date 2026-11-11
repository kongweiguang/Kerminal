//! Selection overlay 的文字色与 run 背景回归；不依赖窗口或终端资源。
//!
//! @author kongweiguang

use gpui_kit::{TextStyle, WindowAppearance};
use kerminal_application::preferences::{TerminalColorScheme, ThemeMode};
use kerminal_terminal::snapshot::{TerminalAttributes, TerminalColor, TerminalRgb, TerminalStyle};

use super::overlay::{CellOverlayPaint, TerminalOverlayKind};
use super::style::{apply_overlay_paint, effective_terminal_foreground, terminal_rgb};
use crate::theme::{terminal_palette_for_mode, terminal_selection_style};

/// Selection overlay 只改写文字色并清空 run 背景；背景已由连续轮廓统一填充。
///
/// @author kongweiguang
#[test]
fn selection_overlay_paint_clears_background_and_uses_contrast_fallback() {
    let palette = terminal_palette_for_mode(
        WindowAppearance::Dark,
        ThemeMode::Dark,
        TerminalColorScheme::Kerminal,
    );
    let selection = terminal_selection_style(&palette);
    let low_contrast = TerminalRgb::new(
        selection.background.red.saturating_add(2),
        selection.background.green.saturating_add(2),
        selection.background.blue.saturating_add(2),
    );
    let terminal_style = TerminalStyle {
        foreground: TerminalColor::Rgb {
            red: low_contrast.red,
            green: low_contrast.green,
            blue: low_contrast.blue,
        },
        background: TerminalColor::DefaultBackground,
        underline_color: None,
        attributes: TerminalAttributes::default(),
    };
    let effective = effective_terminal_foreground(terminal_style, &palette);

    let mut run = TextStyle::default().to_run(1);
    run.background_color = Some(gpui_kit::rgb(0x00ff00).into());
    apply_overlay_paint(
        &mut run,
        CellOverlayPaint {
            kind: TerminalOverlayKind::Selection,
            style: None,
        },
        &palette,
        effective,
        selection,
    );
    assert!(run.background_color.is_none());
    assert_eq!(run.color, terminal_rgb(selection.foreground_fallback));

    // 搜索等其它覆盖层仍需要逐 run 背景，不能被 Selection 的清空逻辑误伤。
    let mut search = TextStyle::default().to_run(1);
    apply_overlay_paint(
        &mut search,
        CellOverlayPaint {
            kind: TerminalOverlayKind::Search,
            style: None,
        },
        &palette,
        effective,
        selection,
    );
    assert!(search.background_color.is_some());
}
