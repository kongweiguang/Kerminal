//! Profile 扫描建议的对照视图。
//!
//! @author kongweiguang

use gpui_kit::{
    Context, IntoElement, SharedString, WeakEntity, Window,
    component::{
        WindowExt as _,
        button::{Button, ButtonVariants},
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::preferences::ProfileScanConflict;

use crate::{features::preferences::panel::PreferencesPanel, theme::ThemeTokens};

impl PreferencesPanel {
    /// 对照只展示会被覆盖的字段；淡入服从减少动态效果设置，关闭和 Esc 由 Kit 恢复焦点。
    /// @author kongweiguang
    pub(super) fn review_profile_scan(
        &self,
        theme: ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_rescan_profiles() || self.profile_scan.conflicts.is_empty() {
            return;
        }
        let conflicts = self.profile_scan.conflicts.clone();
        let panel = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, window, _| {
            let single = match conflicts.as_slice() {
                [conflict] => Some(conflict),
                _ => None,
            };
            dialog
                .entrance(gpui_kit::component::dialog::DialogEntrance::Fade)
                .title(crate::locale::text(
                    "核对启动设置",
                    "Review startup settings",
                ))
                .width(px(560.))
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "覆盖仅更新 Shell 路径和启动参数。",
                            "Replacing updates only the shell path and arguments.",
                        )),
                )
                .child(
                    div()
                        .id("profile-scan-conflicts")
                        .max_h(window.viewport_size().height * 0.45)
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .children(conflicts.iter().map(|conflict| {
                            scan_conflict_row(
                                conflict,
                                theme,
                                single
                                    .is_none()
                                    .then(|| scan_overwrite_button(conflict, panel.clone())),
                            )
                        })),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("profile-scan-close")
                                .label(crate::locale::text("关闭", "Close"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .when_some(single, |footer, conflict| {
                            footer.child(scan_overwrite_button(conflict, panel.clone()).primary())
                        }),
                )
        });
    }
}

/// 对照区使用同一组内边距；多项核对时按钮跟随各自对象，单项由对话框底部承载操作。
/// @author kongweiguang
fn scan_conflict_row(
    conflict: &ProfileScanConflict,
    theme: ThemeTokens,
    action: Option<Button>,
) -> impl IntoElement {
    let current = &conflict.current().profile;
    let suggested = conflict.suggested();
    let comparison = div()
        .flex()
        .flex_col()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .child(scan_launch_values(
            crate::locale::text("当前配置", "Current"),
            &current.shell,
            &current.args,
            theme,
        ))
        .child(
            div()
                .border_t_1()
                .border_color(theme.border)
                .child(scan_launch_values(
                    crate::locale::text("扫描结果", "Discovered"),
                    &suggested.shell,
                    &suggested.args,
                    theme,
                )),
        );
    let name = current.name.clone();
    div()
        .flex_shrink_0()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(name),
        )
        .child(comparison)
        .when_some(action, |row, button| {
            row.child(div().flex().justify_end().child(button.compact()))
        })
}

/// 确认操作绑定到正在核对的 Profile 和原 revision，避免多个建议间误写。
/// @author kongweiguang
fn scan_overwrite_button(
    conflict: &ProfileScanConflict,
    panel: WeakEntity<PreferencesPanel>,
) -> Button {
    let current = &conflict.current().profile;
    let id = format!("profile-scan-overwrite-{}", current.id);
    let action_label = crate::locale::text(
        format!("覆盖 {} 的启动设置", current.name),
        format!("Replace startup settings for {}", current.name),
    );
    let conflict = conflict.clone();
    Button::new(id)
        .label(crate::locale::text(
            "覆盖启动设置",
            "Replace startup settings",
        ))
        .accessibility_label(action_label)
        .on_click(move |_, window, cx| {
            window.close_dialog(cx);
            let _ = panel.update(cx, |panel, cx| {
                panel.overwrite_scanned_profile(conflict.clone(), window, cx);
            });
        })
}

/// 路径与参数分行；逐个保留引号与转义，便于辨认空参数和参数边界。
/// @author kongweiguang
fn scan_launch_values(
    label: SharedString,
    shell: &str,
    args: &[String],
    theme: ThemeTokens,
) -> impl IntoElement {
    let arguments: SharedString = if args.is_empty() {
        crate::locale::text("无启动参数", "No arguments")
    } else {
        args.iter()
            .map(|arg| format!("{arg:?}"))
            .collect::<Vec<_>>()
            .join("  ")
            .into()
    };
    div()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .text_sm()
        .child(
            div()
                .text_color(theme.field_label)
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(label),
        )
        .child(div().min_w_0().child(shell.to_owned()))
        .child(div().min_w_0().text_color(theme.muted).child(arguments))
}
