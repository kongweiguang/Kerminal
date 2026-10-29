//! 屏幕取色结果、取消与规则编辑会话的异步生命周期回归。
//!
//! @author kongweiguang

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use gpui_kit::{
    Entity, VisualTestContext,
    component::{Colorize as _, color_picker::ColorPickerState},
};
use kerminal_application::{
    AppError, AppResult,
    preferences::{
        TerminalKeywordHighlightStyle,
        color_sampler::{
            ColorSampleCancellation, ColorSampleOutcome, SampledColor, ScreenColorSampler,
        },
    },
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{PreferencesPanel, click, draw, fixture, pump_runtime};

#[derive(Clone)]
struct DeferredSampler {
    requests: Arc<Mutex<Vec<PendingSample>>>,
}

struct PendingSample {
    timeout: Duration,
    cancellation: CancellationToken,
    response: oneshot::Sender<AppResult<ColorSampleOutcome>>,
}

impl DeferredSampler {
    /// 每次请求都能由测试单独回执，从而稳定安排取消与迟到结果的先后顺序。
    fn new() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 取出已进入 fake 服务的请求，避免测试对异步任务的启动时序作猜测。
    fn take_request(&self) -> Option<PendingSample> {
        self.requests
            .lock()
            .expect("sampler request queue is not poisoned")
            .pop()
    }
}

#[async_trait]
impl ScreenColorSampler for DeferredSampler {
    /// 等待测试显式回执且故意不因取消 token 提前返回，以模拟取消后才到达的平台结果。
    async fn sample(
        &self,
        timeout: Duration,
        cancellation: CancellationToken,
    ) -> AppResult<ColorSampleOutcome> {
        let (response, result) = oneshot::channel();
        self.requests
            .lock()
            .expect("sampler request queue is not poisoned")
            .push(PendingSample {
                timeout,
                cancellation,
                response,
            });
        result
            .await
            .map_err(|_| AppError::invalid("测试取色回执被丢弃"))?
    }
}

impl PendingSample {
    /// 将平台回执放入请求 future；请求已结束时忽略测试清理产生的发送失败。
    fn resolve(self, result: AppResult<ColorSampleOutcome>) {
        let _ = self.response.send(result);
    }
}

/// 注入 fake 服务并打开新规则的浅色文字 picker，保留一个可比较的现有颜色草稿。
fn prepare_sample(
    panel: &Entity<PreferencesPanel>,
    visual: &mut VisualTestContext,
    sampler: DeferredSampler,
) -> Entity<ColorPickerState> {
    visual.update(|_, cx| {
        panel.update(cx, |panel, _| {
            panel.attach_screen_color_sampler(Some(Arc::new(sampler)));
        });
    });
    click(visual, "preferences-nav-keywords");
    click(visual, "keyword-new");
    let picker = visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.set_keyword_rule_style(TerminalKeywordHighlightStyle::Custom, window, cx);
            let (target, picker) = panel
                .keyword_editor
                .colors
                .iter()
                .find(|field| field.target.id_part() == "light-foreground")
                .map(|field| (field.target, field.picker.clone()))
                .expect("light foreground picker exists");
            panel.update_keyword_editor_color(target, Some("#112233".to_owned()), window, cx);
            picker.update(cx, |state, cx| state.set_open(true, cx));
            picker
        })
    });
    draw(visual);
    picker
}

/// 通过真实 panel 异步入口开始采样，并让 GPUI 与应用 runtime 启动服务 future。
fn start_sample(
    panel: &Entity<PreferencesPanel>,
    picker: &Entity<ColorPickerState>,
    visual: &mut VisualTestContext,
) {
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.sample_keyword_screen_color(picker.clone(), window, cx)
        });
    });
    draw(visual);
}

/// 有界地推进现有 UI 测试 runtime，直到 fake 服务收到采样请求。
fn wait_for_request(
    runtime: &tokio::runtime::Runtime,
    visual: &mut VisualTestContext,
    sampler: &DeferredSampler,
) -> PendingSample {
    for _ in 0..16 {
        pump_runtime(runtime, visual);
        if let Some(request) = sampler.take_request() {
            return request;
        }
    }
    panic!("screen color sampler did not receive the request");
}

/// 读取当前编辑会话里的自定义浅色文字值，用于证明采样结果按规则归属且失败不会清空草稿。
fn draft_light_foreground(
    panel: &Entity<PreferencesPanel>,
    visual: &mut VisualTestContext,
) -> Option<String> {
    visual.update(|_, cx| {
        panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .and_then(|session| session.draft.custom_colors.as_ref())
            .and_then(|colors| colors.light.foreground.clone())
    })
}

/// 读取 picker 的展示色，作为会话切换后验证迟到结果没有触碰共享控件的稳定基线。
fn displayed_color(
    picker: &Entity<ColorPickerState>,
    visual: &mut VisualTestContext,
) -> Option<gpui_kit::Hsla> {
    visual.update(|_, cx| picker.read(cx).displayed_color())
}

/// 真实颜色样本写入当前草稿，并关闭发起请求时打开的 picker。
#[gpui_kit::test]
fn selected_sample_updates_current_keyword_color_draft(app: &mut gpui_kit::TestAppContext) {
    let (runtime, panel, _, visual) = fixture(app);
    let sampler = DeferredSampler::new();
    let picker = prepare_sample(&panel, visual, sampler.clone());
    start_sample(&panel, &picker, visual);
    let request = wait_for_request(&runtime, visual, &sampler);

    assert_eq!(request.timeout, Duration::from_secs(30));
    assert!(!request.cancellation.is_cancelled());
    assert!(!visual.update(|_, cx| picker.read(cx).is_open()));
    request.resolve(Ok(ColorSampleOutcome::Selected(SampledColor::new(
        0x12, 0xab, 0xef,
    ))));
    pump_runtime(&runtime, visual);

    assert_eq!(
        draft_light_foreground(&panel, visual).as_deref(),
        Some("#12ABEF")
    );
    assert_eq!(
        displayed_color(&picker, visual).unwrap().to_hex(),
        "#12ABEF"
    );
}

/// 平台主动取消属于正常结束，不清除既有草稿颜色，也不显示错误状态。
#[gpui_kit::test]
fn cancelled_sample_preserves_existing_color_without_error(app: &mut gpui_kit::TestAppContext) {
    let (runtime, panel, _, visual) = fixture(app);
    let sampler = DeferredSampler::new();
    let picker = prepare_sample(&panel, visual, sampler.clone());
    start_sample(&panel, &picker, visual);
    let request = wait_for_request(&runtime, visual, &sampler);
    request.resolve(Ok(ColorSampleOutcome::Cancelled(
        ColorSampleCancellation::Escape,
    )));
    pump_runtime(&runtime, visual);

    assert_eq!(
        draft_light_foreground(&panel, visual).as_deref(),
        Some("#112233")
    );
    assert!(visual.update(|_, cx| panel.read(cx).keyword_editor.error.is_none()));
}

/// 平台失败向编辑器提供错误反馈，但保留用户在取色前输入的规则颜色。
#[gpui_kit::test]
fn sample_error_preserves_existing_color_and_sets_editor_error(app: &mut gpui_kit::TestAppContext) {
    let (runtime, panel, _, visual) = fixture(app);
    let sampler = DeferredSampler::new();
    let picker = prepare_sample(&panel, visual, sampler.clone());
    start_sample(&panel, &picker, visual);
    let request = wait_for_request(&runtime, visual, &sampler);
    request.resolve(Err(AppError::unsupported("测试取色服务失败")));
    pump_runtime(&runtime, visual);

    assert_eq!(
        draft_light_foreground(&panel, visual).as_deref(),
        Some("#112233")
    );
    assert!(visual.update(|_, cx| panel.read(cx).keyword_editor.error.is_some()));
}

/// 取消规则草稿后仍到达的颜色结果必须因 generation 失效，不能重建或改写草稿。
#[gpui_kit::test]
fn late_sample_after_rule_cancel_does_not_restore_or_change_draft(
    app: &mut gpui_kit::TestAppContext,
) {
    let (runtime, panel, _, visual) = fixture(app);
    let sampler = DeferredSampler::new();
    let picker = prepare_sample(&panel, visual, sampler.clone());
    start_sample(&panel, &picker, visual);
    let request = wait_for_request(&runtime, visual, &sampler);

    click(visual, "keyword-cancel");
    assert!(request.cancellation.is_cancelled());
    draw(visual);
    let color_after_cancel = displayed_color(&picker, visual);
    let draft_after_cancel = draft_light_foreground(&panel, visual);
    request.resolve(Ok(ColorSampleOutcome::Selected(SampledColor::new(
        0x44, 0x55, 0x66,
    ))));
    pump_runtime(&runtime, visual);

    assert!(visual.update(|_, cx| panel.read(cx).keyword_editor.session.is_none()));
    assert_eq!(draft_light_foreground(&panel, visual), draft_after_cancel);
    assert_eq!(displayed_color(&picker, visual), color_after_cancel);
}

/// 切出关键词页面会取消采样并丢弃临时编辑会话；旧回执不能修改仍被其他页面持有的 picker。
#[gpui_kit::test]
fn late_sample_after_page_switch_does_not_change_picker(app: &mut gpui_kit::TestAppContext) {
    let (runtime, panel, _, visual) = fixture(app);
    let sampler = DeferredSampler::new();
    let picker = prepare_sample(&panel, visual, sampler.clone());
    start_sample(&panel, &picker, visual);
    let request = wait_for_request(&runtime, visual, &sampler);

    click(visual, "preferences-nav-terminal");
    assert!(request.cancellation.is_cancelled());
    draw(visual);
    let color_after_page_switch = displayed_color(&picker, visual);
    let draft_after_page_switch = draft_light_foreground(&panel, visual);
    request.resolve(Ok(ColorSampleOutcome::Selected(SampledColor::new(
        0x44, 0x55, 0x66,
    ))));
    pump_runtime(&runtime, visual);

    assert_eq!(displayed_color(&picker, visual), color_after_page_switch);
    assert_eq!(
        draft_light_foreground(&panel, visual),
        draft_after_page_switch
    );
    assert!(visual.update(|_, cx| panel.read(cx).keyword_editor.session.is_none()));
}

/// 替换为另一条新规则会推进采样代际，旧颜色不能落到新的编辑身份上。
#[gpui_kit::test]
fn late_sample_after_rule_replacement_does_not_change_new_draft(
    app: &mut gpui_kit::TestAppContext,
) {
    let (runtime, panel, _, visual) = fixture(app);
    let sampler = DeferredSampler::new();
    let picker = prepare_sample(&panel, visual, sampler.clone());
    let old_rule_id = visual.update(|_, cx| {
        panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .expect("first keyword rule draft exists")
            .draft
            .id
            .clone()
    });
    start_sample(&panel, &picker, visual);
    let request = wait_for_request(&runtime, visual, &sampler);

    click(visual, "keyword-new");
    assert!(request.cancellation.is_cancelled());
    let new_rule_id = visual.update(|_, cx| {
        panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .expect("replacement keyword rule draft exists")
            .draft
            .id
            .clone()
    });
    assert_ne!(new_rule_id, old_rule_id);
    draw(visual);
    let color_after_replacement = displayed_color(&picker, visual);
    let draft_after_replacement = draft_light_foreground(&panel, visual);
    request.resolve(Ok(ColorSampleOutcome::Selected(SampledColor::new(
        0x44, 0x55, 0x66,
    ))));
    pump_runtime(&runtime, visual);

    assert_eq!(
        draft_light_foreground(&panel, visual),
        draft_after_replacement
    );
    assert_eq!(displayed_color(&picker, visual), color_after_replacement);
}
