//! 原生窗口多显示器恢复几何测试。
//!
//! @author kongweiguang

use kerminal_application::platform::window_state::WindowRect;
use kerminal_ui::window_state::restore_rect_for_displays;

/// 从已拔除的屏幕恢复时，将窗口尺寸限制到主屏并居中到可操作区域。
#[test]
fn offscreen_window_is_centered_on_primary_display() {
    let primary = WindowRect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };
    let restored = restore_rect_for_displays(
        WindowRect {
            x: 4200,
            y: 900,
            width: 1440,
            height: 900,
        },
        &[primary],
    );

    assert_eq!(
        restored,
        WindowRect {
            x: 240,
            y: 90,
            width: 1440,
            height: 900,
        }
    );
}

/// 保留副屏上可拖动的标题栏，不因主屏在列表首位而错误搬移当前窗口。
#[test]
fn visible_secondary_display_keeps_its_window_position() {
    let primary = WindowRect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };
    let secondary = WindowRect {
        x: -1600,
        y: 0,
        width: 1600,
        height: 900,
    };
    let rect = WindowRect {
        x: -1500,
        y: 120,
        width: 1280,
        height: 800,
    };

    assert_eq!(restore_rect_for_displays(rect, &[primary, secondary]), rect);
}

/// 极端持久化尺寸不得跨越所有显示器最大范围，避免窗口恢复后不可操作。
#[test]
fn restored_window_is_bounded_by_available_display_area() {
    let display = WindowRect {
        x: 0,
        y: 0,
        width: 800,
        height: 600,
    };
    let restored = restore_rect_for_displays(
        WindowRect {
            x: 9000,
            y: 9000,
            width: 4000,
            height: 2400,
        },
        &[display],
    );

    assert_eq!(
        restored,
        WindowRect {
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }
    );
}
