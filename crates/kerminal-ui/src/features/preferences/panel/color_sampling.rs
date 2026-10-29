//! 色盘采样请求归属于当前规则编辑会话，关闭和取消后不接受迟到结果。
//!
//! @author kongweiguang

use std::{sync::Arc, time::Duration};

use gpui_kit::{Context, Entity, Window, component::color_picker::ColorPickerState};
use kerminal_application::preferences::color_sampler::{ColorSampleOutcome, ScreenColorSampler};
use tokio_util::sync::CancellationToken;

use super::PreferencesPanel;

#[derive(Default)]
pub(super) struct ScreenColorSamplingState {
    service: Option<Arc<dyn ScreenColorSampler>>,
    cancellation: Option<CancellationToken>,
    generation: u64,
}

impl ScreenColorSamplingState {
    /// 取消只撤销当前临时采样，代际号阻止已排队的旧结果写入新规则。
    pub(super) fn cancel(&mut self) -> bool {
        let Some(cancellation) = self.cancellation.take() else {
            return false;
        };
        cancellation.cancel();
        self.generation = self.generation.wrapping_add(1);
        true
    }
}

impl Drop for ScreenColorSamplingState {
    /// 面板销毁也必须结束后台轮询；平台仍有独立超时作为最终资源边界。
    fn drop(&mut self) {
        self.cancel();
    }
}

impl PreferencesPanel {
    /// 组合根注入一次性能力；缺少平台支持时不自行构造替代系统服务。
    pub(crate) fn attach_screen_color_sampler(
        &mut self,
        service: Option<Arc<dyn ScreenColorSampler>>,
    ) {
        self.screen_color_sampling.service = service;
    }

    /// 取色请求冻结规则身份，平台只返回 RGB；最终变更沿用色盘原有草稿事件链。
    pub(in crate::features::preferences) fn sample_keyword_screen_color(
        &mut self,
        picker: Entity<ColorPickerState>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked() || self.keyword_editor.save_pending.is_some() {
            return;
        }
        let Some(rule_id) = self
            .keyword_editor
            .session
            .as_ref()
            .map(|session| session.draft.id.clone())
        else {
            return;
        };
        let Some(service) = self.screen_color_sampling.service.clone() else {
            self.keyword_editor.error = Some(
                crate::locale::text(
                    "屏幕取色服务不可用。",
                    "Screen color sampling is unavailable.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        self.screen_color_sampling.cancel();
        let cancellation = CancellationToken::new();
        self.screen_color_sampling.generation =
            self.screen_color_sampling.generation.wrapping_add(1);
        let generation = self.screen_color_sampling.generation;
        self.screen_color_sampling.cancellation = Some(cancellation.clone());
        picker.update(cx, |picker, cx| picker.set_open(false, cx));
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result =
                executor
                    .run(move |_| async move {
                        service.sample(Duration::from_secs(30), cancellation).await
                    })
                    .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                if generation != panel.screen_color_sampling.generation {
                    return;
                }
                panel.screen_color_sampling.cancellation = None;
                if panel
                    .keyword_editor
                    .session
                    .as_ref()
                    .is_none_or(|session| session.draft.id != rule_id)
                {
                    return;
                }
                match result {
                    Ok(ColorSampleOutcome::Selected(color)) => {
                        let rgb = (u32::from(color.red) << 16)
                            | (u32::from(color.green) << 8)
                            | u32::from(color.blue);
                        picker.update(cx, |picker, cx| {
                            picker.update_color(gpui_kit::rgb(rgb).into(), window, cx)
                        });
                    }
                    Ok(ColorSampleOutcome::Cancelled(_)) => {}
                    Err(error) => panel.keyword_editor.error = Some(super::localized_error(&error)),
                }
                cx.notify();
            });
        })
        .detach();
    }
}
