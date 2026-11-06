//! 设置事务的固定异常反馈；恢复继续由原偏好 owner 和 typed 用例处理。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, ClickEvent, Context, IntoElement,
    component::{Disableable as _, button::Button},
    div,
    prelude::*,
    px,
};

use super::{
    DraftStatus,
    panel::{PreferencesPanel, PreferencesSection},
};
use crate::theme::ThemeTokens;

impl PreferencesPanel {
    /// 异常反馈独立于正文滚动；终端沿用已有固定底栏，避免一份事务出现两份错误通知。
    /// 无异常时不占空间，普通业务 status 和成功通知仍由原页面决定。
    /// @author kongweiguang
    pub(super) fn render_save_recovery(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.settings.as_ref()?.status();
        if self.section == PreferencesSection::Terminal
            || (self.settings_error.is_none()
                && !matches!(
                    state,
                    DraftStatus::Failed
                        | DraftStatus::ExternalConflict
                        | DraftStatus::OutcomeUnknown
                ))
        {
            return None;
        }
        let fallback = match state {
            DraftStatus::OutcomeUnknown => crate::locale::text(
                "保存结果尚未确认。更改已保留，请读回核对。",
                "The save outcome is unconfirmed. Your changes are preserved; read back to verify.",
            ),
            DraftStatus::ExternalConflict => crate::locale::text(
                "设置已在外部更改。更改已保留，请读回核对。",
                "Settings changed externally. Your changes are preserved; read back to verify.",
            ),
            _ => crate::locale::text(
                "未能保存。更改已保留，可重试。",
                "Could not save. Your changes are preserved; you can retry.",
            ),
        };
        let message = self
            .settings_error
            .clone()
            .unwrap_or_else(|| fallback.to_string());
        Some(
            div()
                .id("preferences-save-recovery")
                .flex_shrink_0()
                .mt_2()
                .pt_2()
                .border_t_1()
                .border_color(theme.border)
                .flex()
                .items_start()
                .gap_2()
                .text_size(theme.text_xs)
                .text_color(theme.error)
                .child(
                    div()
                        .id("preferences-save-recovery-message")
                        .flex_1()
                        .min_w_0()
                        .max_h(px(96.))
                        .overflow_y_scroll()
                        .child(message),
                )
                .when_some(self.render_save_recovery_action(cx), |row, action| {
                    row.child(action)
                })
                .into_any_element(),
        )
    }

    /// 确定失败才重试保存；冲突与未知结果只读回，读取忙态在原请求回执中释放。
    /// 终端保留原稳定按钮身份；受理后立即交还正文焦点，避免成功撤掉按钮时留下无效焦点。
    /// @author kongweiguang
    pub(super) fn render_save_recovery_action(&self, cx: &mut Context<Self>) -> Option<Button> {
        let draft = self.settings.as_ref()?;
        match draft.status() {
            DraftStatus::Failed
                if draft.is_dirty()
                    && self.settings_input_errors.is_empty()
                    && !self.settings_issues.iter().any(|issue| issue.blocks_save())
                    && !self.settings_controls_locked() =>
            {
                let id = if self.section == PreferencesSection::Terminal {
                    "terminal-retry-save"
                } else {
                    "preferences-retry-save"
                };
                Some(
                    Button::new(id)
                        .compact()
                        .outline()
                        .label(crate::locale::text("重试保存", "Retry save"))
                        .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                            if panel.save_settings(cx) {
                                window.focus(&panel.settings_content_focus, cx);
                            }
                        })),
                )
            }
            DraftStatus::OutcomeUnknown | DraftStatus::ExternalConflict => Some(
                Button::new("preferences-read-back-settings")
                    .compact()
                    .outline()
                    .label(crate::locale::text("读回核对", "Read back"))
                    .loading(self.settings_readback_running)
                    .disabled(self.loading_settings || self.settings_readback_running)
                    .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                        panel.recover_pending_settings_on_open(cx);
                        window.focus(&panel.settings_content_focus, cx);
                    })),
            ),
            _ => None,
        }
    }
}
