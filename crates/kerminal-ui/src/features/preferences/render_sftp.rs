//! GPUI SFTP 设置页：按 main 保留基础并发与可折叠的高级传输参数。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, Entity, IntoElement, SharedString, Window,
    assets::IconName,
    component::{
        Disableable as _, Icon,
        button::{Button, ButtonVariants as _},
        input::InputState,
    },
    div,
    prelude::*,
    px,
};

use crate::theme::ThemeTokens;

use super::panel::PreferencesPanel;

impl PreferencesPanel {
    /// 搜索命中高级参数时先展开折叠区，避免焦点落在不可见控件上。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn reveal_sftp_advanced(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if !self.sftp_advanced_open {
            self.sftp_advanced_open = true;
            cx.notify();
        }
    }

    /// 复用原输入实体保留数字编辑中间态；列数依 main 的视口 md 断点而非卡片可用宽度确定。
    ///
    /// @author kongweiguang
    pub(super) fn render_sftp(
        &mut self,
        theme: ThemeTokens,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return self.render_settings_loading(theme).into_any_element();
        };
        let locked = self.settings_controls_locked();
        let medium_layout = window.viewport_size().width.as_f32() >= 768.;
        let sftp = settings.sftp;
        let timeout_summary = format_idle_timeout_summary(sftp.idle_timeout_seconds);

        let content = div()
            .id("preferences-sftp")
            .flex_1()
            .w_full()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.content_scroll)
            .flex()
            .flex_col()
            .gap_4()
            .child(
                sftp_panel(crate::locale::text("SFTP 传输", "SFTP transfers"), theme)
                    .child(
                        div()
                            .w_full()
                            .text_size(theme.text_xs)
                            .line_height(px(20.))
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "参数仅影响新任务。",
                                "Settings apply to new transfers only.",
                            )),
                    )
                    .child(
                        div()
                            .w_full()
                            .grid()
                            .grid_cols(if medium_layout { 2 } else { 1 })
                            .gap_3()
                            .child(
                                div()
                                    .id("sftp-global-transfers")
                                    .flex_1()
                                    .min_w(px(0.))
                                    .child(sftp_number_setting(
                                        crate::locale::text("全局传输并发", "Global transfers"),
                                        &self.settings_inputs.sftp_global_transfers,
                                        None,
                                        Some(crate::locale::text("所有任务共享。", "Shared by all transfers.")),
                                        locked,
                                        theme,
                                    )),
                            )
                            .child(
                                div()
                                    .id("sftp-host-transfers")
                                    .flex_1()
                                    .min_w(px(0.))
                                    .child(sftp_number_setting(
                                        crate::locale::text("单主机并发", "Per-host transfers"),
                                        &self.settings_inputs.sftp_host_transfers,
                                        None,
                                        Some(crate::locale::text("限制单机压力。", "Limits load on one host.")),
                                        locked,
                                        theme,
                                    )),
                            ),
                    ),
            )
            .child(
            div()
                    .w_full()
                    .rounded(theme.radius_panel)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.panel)
                    .flex()
                    .flex_col()
                    .child(
                        Button::new("preferences-sftp-advanced")
                            .compact()
                            .custom(super::render_settings::settings_ghost_variant(theme, cx))
                            .w_full()
                            .min_h(px(44.))
                            .px_3()
                            .py_2()
                            .accessibility_label(crate::locale::text(
                                "高级传输参数",
                                "Advanced transfer settings",
                            ))
                            .disabled(locked)
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_3()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                Icon::new(if self.sftp_advanced_open {
                                                    IconName::ChevronDown
                                                } else {
                                                    IconName::ChevronRight
                                                })
                                                .size(px(16.))
                                                .text_color(theme.muted),
                                            )
                                            .child(
                                                div()
                                                    .text_size(theme.text_sm)
                                                    .font_weight(
                                                        theme.settings_medium_font_weight(),
                                                    )
                                                    .text_color(theme.foreground)
                                                    .child(crate::locale::text(
                                                        "高级传输参数",
                                                        "Advanced transfer settings",
                                                    )),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .truncate()
                                            .text_size(theme.text_xs)
                                            .text_color(theme.muted)
                                            .text_right()
                                            .child(timeout_summary),
                                    ),
                            )
                            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                panel.sftp_advanced_open = !panel.sftp_advanced_open;
                                cx.notify();
                            })),
                    )
                    .when(self.sftp_advanced_open, |body| {
                        body.child(
                            div()
                                .w_full()
                                .border_t_1()
                                .border_color(theme.border)
                                .p_3()
                                .grid()
                                .grid_cols(if medium_layout { 3 } else { 1 })
                                .gap_3()
                                .child(
                                    div()
                                        .id("sftp-pipeline-depth")
                                        .flex_1()
                                        .min_w(px(0.))
                                        .child(sftp_number_setting(
                                            crate::locale::text("流水线深度", "Pipeline depth"),
                                            &self.settings_inputs.sftp_pipeline_depth,
                                            None,
                                            Some(crate::locale::text("提高吞吐，也增加压力。", "Higher throughput increases load.")),
                                            locked,
                                            theme,
                                        )),
                                )
                                .child(
                                    div()
                                        .id("sftp-packet-bytes")
                                        .flex_1()
                                        .min_w(px(0.))
                                        .child(sftp_number_setting(
                                            crate::locale::text("最大包大小", "Maximum packet size"),
                                            &self.settings_inputs.sftp_packet_bytes,
                                            Some("M".into()),
                                            Some(crate::locale::text("单位 M；0.25 = 256K。", "Unit M; 0.25 = 256K.")),
                                            locked,
                                            theme,
                                        )),
                                )
                                .child(
                                    div()
                                        .id("sftp-idle-timeout")
                                        .flex_1()
                                        .min_w(px(0.))
                                        .child(sftp_number_setting(
                                            crate::locale::text("无进度超时", "No-progress timeout"),
                                            &self.settings_inputs.sftp_idle_timeout,
                                            Some(crate::locale::text("秒", "sec")),
                                            Some(crate::locale::text(
                                                "仅在连续无数据时停止，不限制文件大小或总时长。",
                                                "Stops only after a period without data; file size and total duration are unlimited.",
                                            )),
                                            locked,
                                            theme,
                                        )),
                                ),
                        )
                    }),
            )
            .children(
                self.settings_issues
                    .iter()
                    .map(|issue| super::render_settings::settings_issue_row(issue, theme)),
            )
            .children(
                self.settings_input_errors
                    .values()
                    .map(|message| super::render_settings::issue_row(message, theme)),
            )
            .children(super::render::settings_error_banner(
                self.settings_error.as_deref(),
                theme,
            ))
            .child(self.render_save_notice(theme));

        content.into_any_element()
    }
}

/// 普通单行 Input 复用既有 InputState 订阅，去掉步进器而保留原始文本、校验和自动保存边界。
///
/// @author kongweiguang
fn sftp_number_setting(
    label: SharedString,
    input: &Entity<InputState>,
    suffix: Option<SharedString>,
    help: Option<SharedString>,
    disabled: bool,
    theme: ThemeTokens,
) -> impl IntoElement {
    let field = super::render_settings::settings_input(input, theme)
        .disabled(disabled)
        .aria_label(label.clone())
        .when_some(suffix, |field, suffix| {
            field.suffix(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(suffix),
            )
        });
    let field = gpui_kit::Styled::h(field, px(36.)).w_full();

    div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(theme.text_xs)
                .font_weight(theme.settings_medium_font_weight())
                .text_color(theme.field_label)
                .child(label),
        )
        .child(field)
        .when_some(help, |field, help| {
            field.child(
                div()
                    .text_size(theme.text_xs)
                    .line_height(px(20.))
                    .text_color(theme.muted)
                    .child(help),
            )
        })
}

/// SFTP 卡片复用 main 的网络图标、标题层级和统一表面。
///
/// @author kongweiguang
fn sftp_panel(title: impl Into<SharedString>, theme: ThemeTokens) -> gpui_kit::Div {
    div()
        .w_full()
        .min_w(px(0.))
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
        .bg(theme.panel)
        .rounded(theme.radius_panel)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    Icon::new(IconName::Network)
                        .size(px(16.))
                        .text_color(theme.accent),
                )
                .child(
                    div()
                        .text_size(theme.text_sm)
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(title.into()),
                ),
        )
}

/// 用“无进度保护”描述 idle timeout，避免被误解为限制传输总时长。
///
/// @author kongweiguang
fn format_idle_timeout_summary(seconds: u16) -> SharedString {
    let (zh, en) = if seconds.is_multiple_of(60) {
        (
            format!("{} 分钟无进度保护", seconds / 60),
            format!("{} min no-progress protection", seconds / 60),
        )
    } else {
        (
            format!("{seconds} 秒无进度保护"),
            format!("{seconds} sec no-progress protection"),
        )
    };
    crate::locale::text(
        format!("总时长不限 · {zh}"),
        format!("No total duration limit · {en}"),
    )
}
