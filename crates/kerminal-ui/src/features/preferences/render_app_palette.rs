//! 应用色板选择与轻量预览；终端色盘由其独立设置页负责。
//!
//! @author kongweiguang

use gpui_kit::{
    Context, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Styled as _,
    WindowAppearance,
    component::{
        FocusableExt as _,
        radio::{Radio, RadioGroup},
    },
    div,
    prelude::FluentBuilder as _,
    relative, rems,
};
use kerminal_application::preferences::{AppColorScheme, ThemeMode};

use crate::theme::ThemeTokens;

use super::panel::PreferencesPanel;

/// 使用稳定的内置色板顺序和 Kit 单选语义，所有选择都进入现有草稿及保存流程。
/// 横向组由其实际内容宽度自动换行，避免设置弹框宽度与窗口宽度不同时出现裁剪。
///
/// @author kongweiguang
pub(super) fn app_color_scheme_choices(
    selected: AppColorScheme,
    mode: ThemeMode,
    appearance: WindowAppearance,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<PreferencesPanel>,
) -> RadioGroup {
    let radios = AppColorScheme::ALL
        .into_iter()
        .enumerate()
        .map(|(index, scheme)| {
            let name = color_scheme_name(scheme);
            let preview = ThemeTokens::for_scheme(appearance, mode, scheme);
            let selected_here = scheme == selected;
            Radio::new(scheme.as_str())
                .debug_selector(move || format!("app-color-{}", scheme.as_str()))
                .w(rems(7.))
                .max_w(relative(1.))
                .min_w(rems(0.))
                .p_2()
                .rounded(theme.control_radius)
                .bg(if selected_here {
                    theme.surface_selected()
                } else {
                    theme.muted_surface()
                })
                .when(!disabled, |radio| {
                    radio.hover(move |style| {
                        style.bg(if selected_here {
                            theme.surface_selected()
                        } else {
                            theme.surface_hover()
                        })
                    })
                })
                .when(disabled, |radio| radio.opacity(0.6))
                .label(name)
                .accessibility_label(crate::locale::text(
                    format!("应用配色：{name}"),
                    format!("App color scheme: {name}"),
                ))
                .focus_ring(true)
                .on_key_down(cx.listener(move |panel, event: &KeyDownEvent, window, cx| {
                    if disabled || event.keystroke.modifiers.modified() {
                        return;
                    }
                    let key = event.keystroke.key.to_ascii_lowercase();
                    let Some((target, forward, steps)) = palette_key_target(index, &key) else {
                        return;
                    };
                    // 单选项内没有其它 Tab stop；跨首尾时沿组内相反方向回到目标。
                    for _ in 0..steps {
                        if forward {
                            window.focus_next(cx);
                        } else {
                            window.focus_prev(cx);
                        }
                    }
                    if let Some(next) = AppColorScheme::ALL.get(target).copied() {
                        panel.edit_settings(|settings| settings.app_color_scheme = next, cx);
                    }
                    cx.stop_propagation();
                }))
                .child(palette_preview(preview))
        })
        .collect::<Vec<_>>();

    RadioGroup::horizontal("app-color-schemes")
        .w_full()
        .min_w(rems(0.))
        .disabled(disabled)
        .selected_index(
            AppColorScheme::ALL
                .iter()
                .position(|scheme| *scheme == selected),
        )
        .children(radios)
        .on_change(cx.listener(|panel, index: &usize, _, cx| {
            if let Some(next) = AppColorScheme::ALL.get(*index).copied() {
                panel.edit_settings(|settings| settings.app_color_scheme = next, cx);
            }
        }))
}

/// 名称只用于界面展示；稳定身份和配置存储均由枚举自身提供。
///
/// @author kongweiguang
fn color_scheme_name(scheme: AppColorScheme) -> &'static str {
    match scheme {
        AppColorScheme::Kerminal => "Kerminal",
        AppColorScheme::JetBrains => "JetBrains",
        AppColorScheme::Fleet => "Fleet",
        AppColorScheme::Xcode => "Xcode",
        AppColorScheme::Claude => "Claude",
        AppColorScheme::VsCode => "VS Code",
    }
}

/// 缩略图只投影真实语义色，不读取图片、启动新窗口或更改当前全局主题。
///
/// @author kongweiguang
fn palette_preview(theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .h_8()
        .overflow_hidden()
        .rounded(theme.radius_sm)
        .bg(theme.background)
        .flex()
        .child(div().w_3().h_full().bg(theme.sidebar))
        .child(
            div()
                .flex_1()
                .min_w(rems(0.))
                .h_full()
                .p_1()
                .flex()
                .flex_col()
                .gap_1()
                .bg(theme.panel)
                .child(
                    div()
                        .w_6()
                        .h_1()
                        .rounded(theme.radius_sm)
                        .bg(theme.foreground),
                )
                .child(div().w_8().h_1().rounded(theme.radius_sm).bg(theme.muted))
                .child(div().w_4().h_1().rounded(theme.radius_sm).bg(theme.accent)),
        )
}

/// Kit 单选组保留原生 Space/Enter 行为；仅补方向键循环并计算有限焦点步数。
///
/// @author kongweiguang
fn palette_key_target(index: usize, key: &str) -> Option<(usize, bool, usize)> {
    let count = AppColorScheme::ALL.len();
    if index >= count {
        return None;
    }
    match key {
        "left" | "up" if index == 0 => Some((count - 1, true, count - 1)),
        "left" | "up" => Some((index - 1, false, 1)),
        "right" | "down" if index + 1 == count => Some((0, false, count - 1)),
        "right" | "down" => Some((index + 1, true, 1)),
        _ => None,
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/preferences_app_palette.rs"]
mod tests;
