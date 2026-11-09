//! 独立原生窗口使用的十万行虚拟列表与真实 GPUI 帧探针。
//!
//! @author kongweiguang

mod state;

pub use state::{PageCache, PageRequest, PageResponse, ProbeRow};

use std::{
    cell::RefCell,
    ops::Range,
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use gpui_kit::{
    App, Context, IntoElement, Render, ScrollStrategy, Styled, UniformListScrollHandle, Window,
    component::ActiveTheme,
    div,
    prelude::*,
    profiler::{FrameEvent, FrameTimingCollector},
    uniform_list,
};
use serde_json::{Value, json};

/// 与 P0.4 对齐，长度是逻辑行数而不是内存中的预生成行。
pub const ROW_COUNT: usize = 100_000;
const WARMUP_TICKS: usize = 20;
const SCROLL_TICKS: usize = 300;
const MAX_SETTLE_TICKS: usize = 180;
const MIN_FRAME_SAMPLES: usize = 120;
const FRAME_LIMIT_NS: u64 = 16_700_000;

/// 进程结果经内存交还桌面组合根，UI 本身不做磁盘和进程等待。
pub type ProbeResult = Arc<Mutex<Option<Value>>>;

/// 测量视图独占分页、滚动与帧采样状态；关闭应用会释放全部句柄。
pub struct NativeListProbe {
    cache: Rc<RefCell<PageCache>>,
    visible: Rc<RefCell<Option<Range<usize>>>>,
    scroll: UniformListScrollHandle,
    requests: mpsc::Sender<PageRequest>,
    responses: mpsc::Receiver<Result<PageResponse, String>>,
    result: ProbeResult,
    collector: FrameTimingCollector,
    frames: Vec<Value>,
    fetched: Vec<Value>,
    completed_requests: usize,
    stale_viewport_ticks: usize,
    warmup: usize,
    scroll_ticks: usize,
    settle_ticks: usize,
    environment_start: Option<Value>,
}

impl NativeListProbe {
    /// 在首帧之前固定预热页面与 RPC 通道，避免绘制路径承担 I/O。
    pub fn new(
        cache: PageCache,
        requests: mpsc::Sender<PageRequest>,
        responses: mpsc::Receiver<Result<PageResponse, String>>,
        result: ProbeResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.on_next_frame(window, |probe, window, cx| probe.tick(window, cx));
        Self {
            cache: Rc::new(RefCell::new(cache)),
            visible: Rc::new(RefCell::new(None)),
            scroll: UniformListScrollHandle::new(),
            requests,
            responses,
            result,
            collector: FrameTimingCollector::new(),
            frames: Vec::new(),
            fetched: Vec::new(),
            completed_requests: 0,
            stale_viewport_ticks: 0,
            warmup: 0,
            scroll_ticks: 0,
            settle_ticks: 0,
            environment_start: None,
        }
    }

    /// 帧回调只收取已经完成的数据；真正的 JSON-RPC I/O 由桌面后台线程负责。
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.warmup >= WARMUP_TICKS && self.scroll_ticks > 0 && self.settle_ticks == 0 {
            self.collect_frames(window);
        } else {
            self.collector.collect_unseen();
            if self.warmup < WARMUP_TICKS {
                self.warmup += 1;
            }
        }

        loop {
            match self.responses.try_recv() {
                Ok(Ok(page)) => {
                    let accepted = self.cache.borrow_mut().accept(page);
                    if let Err(error) = accepted {
                        self.finish(window, cx, Some(error.to_owned()));
                        return;
                    }
                    self.completed_requests += 1;
                }
                Ok(Err(error)) => {
                    self.finish(window, cx, Some(error));
                    return;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.finish(window, cx, Some("RPC worker disconnected".into()));
                    return;
                }
            }
        }

        let visible = self.visible.borrow().clone();
        if let Some(visible) = visible {
            let requested = {
                let mut cache = self.cache.borrow_mut();
                if !cache.covers(&visible) {
                    self.stale_viewport_ticks += 1;
                }
                cache.request_visible(visible)
            };
            match requested {
                Ok(Some(request)) => {
                    let range = request.range();
                    self.fetched
                        .push(json!({"id": request.id(), "range": [range.start, range.end]}));
                    if self.requests.send(request).is_err() {
                        self.finish(window, cx, Some("RPC worker rejected request".into()));
                        return;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.finish(window, cx, Some(error.to_owned()));
                    return;
                }
            }
        }

        if self.warmup >= WARMUP_TICKS {
            if self.environment_start.is_none() {
                self.environment_start = Some(window_environment(window, cx));
            }
            if self.scroll_ticks < SCROLL_TICKS {
                let index = (ROW_COUNT - 1) * self.scroll_ticks / (SCROLL_TICKS - 1);
                self.scroll
                    .scroll_to_item_strict(index, ScrollStrategy::Top);
                self.scroll_ticks += 1;
            } else {
                self.settle_ticks += 1;
                let settled = self.visible.borrow().as_ref().is_some_and(|range| {
                    let cache = self.cache.borrow();
                    cache.covers(range) && !cache.is_pending()
                });
                if settled && self.settle_ticks >= 3 {
                    self.finish(window, cx, None);
                    return;
                }
                if self.settle_ticks >= MAX_SETTLE_TICKS {
                    self.finish(window, cx, Some("final visible page did not settle".into()));
                    return;
                }
            }
        }

        cx.notify();
        cx.on_next_frame(window, |probe, window, cx| probe.tick(window, cx));
    }

    /// 只记录指定窗口真正发生的 draw/present 事件，不把页面生成耗时当作帧。
    fn collect_frames(&mut self, window: &Window) {
        let window_id = window.window_handle().window_id();
        for event in self.collector.collect_unseen() {
            match event {
                FrameEvent::Draw(frame) if frame.window_id == window_id => {
                    self.frames.push(json!({
                        "draw_ns": nanos(frame.draw_duration()),
                        "dirty_to_draw_ns": frame.dirty_to_draw_duration().map(nanos),
                        "request_layout_ns": nanos(frame.phases.request_layout),
                        "prepaint_ns": nanos(frame.phases.prepaint),
                        "paint_ns": nanos(frame.phases.paint),
                        "refresh_interval_ns": frame.refresh_interval.map(nanos),
                        "present_ns": null,
                        "animation_interval_ns": null,
                    }));
                }
                FrameEvent::Present(present) if present.window_id == window_id => {
                    if let Some(sample) = self.frames.last_mut() {
                        sample["present_ns"] = json!(nanos(present.present_duration()));
                        sample["animation_interval_ns"] =
                            json!(present.animation_interval.map(nanos));
                    }
                }
                _ => {}
            }
        }
    }

    /// 保留原始帧与分页请求；采样不足和软件 GPU 必须单独标示而不能冒充通过。
    fn finish(&mut self, window: &Window, cx: &mut Context<Self>, error: Option<String>) {
        let draw_ns = self
            .frames
            .iter()
            .filter_map(|frame| frame["draw_ns"].as_u64())
            .collect::<Vec<_>>();
        let interval_ns = self
            .frames
            .iter()
            .filter_map(|frame| frame["animation_interval_ns"].as_u64())
            .collect::<Vec<_>>();
        let draw_p95 = percentile_95(draw_ns);
        let interval_p95 = percentile_95(interval_ns.clone());
        let end = window_environment(window, cx);
        let hardware_gpu = self.environment_start.as_ref().is_some_and(|start| {
            start["gpu"]["is_software_emulated"].as_bool() == Some(false)
                && end["gpu"]["is_software_emulated"].as_bool() == Some(false)
                && start["gpu"] == end["gpu"]
        });
        let measured = error.is_none()
            && self.frames.len() >= MIN_FRAME_SAMPLES
            && interval_ns.len() >= MIN_FRAME_SAMPLES
            && self.completed_requests > 1
            && self.fetched.iter().any(|request| {
                request["range"][0]
                    .as_u64()
                    .is_some_and(|start| start >= 90_000)
            });
        let verdict = if !measured || !hardware_gpu {
            "inconclusive"
        } else if draw_p95.is_some_and(|value| value <= FRAME_LIMIT_NS)
            && interval_p95.is_some_and(|value| value <= FRAME_LIMIT_NS)
        {
            "passed"
        } else {
            "failed"
        };
        let report = json!({
            "schema": 1,
            "verdict": verdict,
            "error": error,
            "logical_rows": ROW_COUNT,
            "scope": "dedicated GPUI fixture; does not exercise production plugin runtime",
            "source": "view.fetch over NDJSON child process; range is half-open",
            "scroll_input": "programmatic GPUI scroll handle",
            "measurement_boundary": "GPUI draw and platform submission during programmatic scroll; not OS scanout",
            "frame_limit_ns": FRAME_LIMIT_NS,
            "draw_p95_ns": draw_p95,
            "animation_interval_p95_ns": interval_p95,
            "draw_sample_count": self.frames.len(),
            "animation_interval_sample_count": interval_ns.len(),
            "raw_frames": std::mem::take(&mut self.frames),
            "rpc_requests": std::mem::take(&mut self.fetched),
            "rpc_completed": self.completed_requests,
            "stale_viewport_ticks": self.stale_viewport_ticks,
            "scroll_ticks": self.scroll_ticks,
            "window_start": self.environment_start.take(),
            "window_end": end,
        });
        if let Ok(mut result) = self.result.lock() {
            *result = Some(report);
        }
        cx.quit();
    }
}

impl Render for NativeListProbe {
    /// 列表由 GPUI 决定可视范围；回调只投影已有行并记录该范围，不发 RPC。
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cache = Rc::clone(&self.cache);
        let visible = Rc::clone(&self.visible);
        let foreground = cx.theme().foreground;
        let background = cx.theme().background;
        let muted = cx.theme().muted_foreground;
        div()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .child(
                uniform_list(
                    "native-list-probe",
                    ROW_COUNT,
                    move |range: Range<usize>, _, _| {
                        *visible.borrow_mut() = Some(range.clone());
                        cache
                            .borrow()
                            .project(range)
                            .into_iter()
                            .map(|row| {
                                div()
                                    .h_8()
                                    .w_full()
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .text_sm()
                                    .child(row.title().to_owned())
                                    .child(div().text_color(muted).child(row.subtitle().to_owned()))
                            })
                            .collect::<Vec<_>>()
                    },
                )
                .track_scroll(&self.scroll)
                .size_full(),
            )
    }
}

/// 使用 GPUI 提供的当前窗口和渲染设备信息，保留采样边界而不推测平台环境。
fn window_environment(window: &Window, cx: &App) -> Value {
    let bounds = window.bounds();
    let viewport = window.viewport_size();
    let scale = window.scale_factor();
    let gpu = window.gpu_specs().map(|gpu| {
        json!({
            "device_name": gpu.device_name,
            "driver_name": gpu.driver_name,
            "driver_info": gpu.driver_info,
            "is_software_emulated": gpu.is_software_emulated,
        })
    });
    json!({
        "window_width_logical_px": f32::from(bounds.size.width),
        "window_height_logical_px": f32::from(bounds.size.height),
        "viewport_width_logical_px": f32::from(viewport.width),
        "viewport_height_logical_px": f32::from(viewport.height),
        "scale_factor": scale,
        "equivalent_dpi_at_96_base": scale * 96.0,
        "font_family": cx.theme().font_family.to_string(),
        "font_size_logical_px": f32::from(cx.theme().font_size),
        "rem_size_logical_px": f32::from(window.rem_size()),
        "gpu": gpu,
    })
}

/// 保留纳秒整数以便外部复算，饱和转换避免极端值截断。
fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// 最近秩定义与原始数据一起输出，空样本不能被报告为零延迟。
fn percentile_95(mut samples: Vec<u64>) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    Some(samples[(samples.len() * 95).div_ceil(100) - 1])
}
