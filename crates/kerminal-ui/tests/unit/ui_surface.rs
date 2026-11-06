//! 共享表面通过真实控件验证换行、文字缩放与草稿身份，不复制业务状态机。
//!
//! @author kongweiguang

use gpui_kit::{
    Context, Entity, IntoElement, Render, TestAppContext, Window,
    component::{
        Theme,
        button::{Button, ButtonVariants as _},
        input::{Input, InputState},
    },
    div,
    prelude::*,
    px, size,
};
use kerminal_application::preferences::{AppColorScheme, InterfaceDensity, ThemeMode};

use crate::theme::{ActiveInterfaceDensity, ThemeTokens, apply_app_theme};

struct SurfaceHarness {
    input: Entity<InputState>,
}

impl Render for SurfaceHarness {
    /// 使用生产共享表面承载实际输入与按钮，确保几何断言覆盖最终布局而非样式副本。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(super::panel_header(theme).child("文件设置"))
            .child(
                super::panel_content(theme)
                    .child(Input::new(&self.input))
                    .child(
                        super::dialog_actions(theme)
                            .id("surface-actions")
                            .child(
                                Button::new("surface-cancel")
                                    .debug_selector(|| "surface-cancel".to_owned())
                                    .label("取消并保留草稿")
                                    .min_w(px(152.))
                                    .flex_shrink_0(),
                            )
                            .child(
                                Button::new("surface-save")
                                    .debug_selector(|| "surface-save".to_owned())
                                    .label("保存并继续")
                                    .primary()
                                    .min_w(px(152.))
                                    .flex_shrink_0(),
                            ),
                    ),
            )
    }
}

/// 窄窗放大后动作换行且完整可见，主题与密度切换不重建输入或清掉草稿。
#[gpui_kit::test]
fn narrow_scaled_actions_wrap_without_losing_input(app: &mut TestAppContext) {
    app.update(crate::init);
    let (view, visual) = app.add_window_view(|window, cx| SurfaceHarness {
        input: cx.new(|cx| InputState::new(window, cx).default_value("保留的草稿")),
    });
    visual.simulate_resize(size(px(300.), px(600.)));
    visual.update(|window, cx| {
        window.set_rem_size(px(24.));
        cx.set_global(ActiveInterfaceDensity(InterfaceDensity::Spacious));
    });
    visual.run_until_parked();
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
        let cancel = window
            .debug_selector_bounds("surface-cancel")
            .expect("cancel is laid out");
        let save = window
            .debug_selector_bounds("surface-save")
            .expect("save is laid out");
        assert!(
            save.top() >= cancel.bottom(),
            "narrow actions wrap rather than overlap"
        );
        for bounds in [cancel, save] {
            let viewport = window.visual_viewport_bounds();
            assert!(bounds.left() >= viewport.left() && bounds.right() <= viewport.right());
            assert!(bounds.bottom() <= viewport.bottom());
        }
        let input = view.read(cx).input.clone();
        view.update(cx, |_, cx| {
            apply_app_theme(
                ThemeMode::Dark,
                AppColorScheme::Kerminal,
                window.appearance(),
                cx,
            );
        });
        cx.set_global(ActiveInterfaceDensity(InterfaceDensity::Compact));
        assert_eq!(view.read(cx).input, input);
        assert_eq!(input.read(cx).value().as_str(), "保留的草稿");
        assert_eq!(ThemeTokens::current(window, cx).text_body, px(21.));
    });
}

/// 主题切换必须保留 Kit 的用户字号，自绘文字与命中区同时服从窗口的实际 rem。
#[gpui_kit::test]
fn switching_palette_preserves_interface_zoom(app: &mut TestAppContext) {
    app.update(crate::init);
    let (view, visual) = app.add_window_view(|window, cx| SurfaceHarness {
        input: cx.new(|cx| InputState::new(window, cx)),
    });
    visual.update(|window, cx| {
        Theme::update(cx, |theme| theme.font_size = px(24.));
        window.set_rem_size(px(24.));
        for mode in [ThemeMode::Dark, ThemeMode::Light, ThemeMode::System] {
            view.update(cx, |_, cx| {
                apply_app_theme(mode, AppColorScheme::Kerminal, window.appearance(), cx);
            });
            assert_eq!(Theme::global(cx).font_size, px(24.));
            let theme = ThemeTokens::current(window, cx);
            assert!(theme.metrics.control_height >= theme.text_body + theme.metrics.gap);
            assert!(theme.metrics.row_height >= theme.metrics.icon_size + theme.metrics.gap);
        }
    });
}
