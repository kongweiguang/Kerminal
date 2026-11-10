//! 系统 WebView 的 GPUI 子窗口宿主；这里只传输消息，不授予业务或文件系统能力。
//!
//! @author kongweiguang

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, OnceLock, mpsc},
    time::{Duration, Instant},
};

use gpui_kit::{
    App, AppContext as _, Bounds, Context, Corners, Entity, EventEmitter, IntoElement, Pixels,
    Render, Subscription, Task, Window, WindowId, canvas, component::WindowExt as _, div,
    prelude::*,
};
use kerminal_application::{AppError, AppResult};
use serde::Serialize;
use serde_json::Value;

mod backdrop;
mod document;
mod occlusion;
mod platform;
mod resources;

pub use resources::{PluginWebViewResource, PluginWebViewResourceDocument, PluginWebViewSource};

const MAX_BRIDGE_BYTES: usize = 64 * 1024;
const MAX_QUEUED_EVENTS: usize = 64;

/// 保留内联平台验收入口；正式包资源使用独立冻结文档，不能继承内联脚本授权。
pub struct PluginWebViewDocument {
    pub instance_id: String,
    pub html: String,
}

/// 原生回调只进入有界队列；业务层仍须校验协议、权限、请求代次和已绑定目标。
#[derive(Clone, Debug)]
pub enum PluginWebViewEvent {
    Message {
        instance_id: String,
        body: String,
    },
    Loaded {
        instance_id: String,
    },
    /// 外部 URL 不交给原生网页导航，宿主须经 open-external 权限路由处理。
    ExternalNavigation {
        instance_id: String,
        url: String,
    },
    Failed {
        instance_id: String,
        message: String,
    },
}

/// 物理像素由 GPUI 当前 scale factor 一次换算，避免 WebView 再按系统 DPI 放大。
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct PluginWebViewBounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// 验收读回真实宿主状态；页面草稿仍由存活的系统 WebView 实例保留。
#[derive(Clone, Debug, Serialize)]
pub struct PluginWebViewSnapshot {
    pub instance_id: String,
    pub native_id: String,
    pub native_parent_id: u64,
    pub visible: bool,
    /// Windows浮层期间引擎继续处理回执，但空区域和禁用输入使其不成为可交互表面。
    pub engine_visible: bool,
    pub loaded: bool,
    pub bounds: Option<PluginWebViewBounds>,
    pub clip: Option<PluginWebViewBounds>,
    pub scale_factor: f32,
    pub message_count: u64,
    pub parent_position_notifications: u64,
    pub failure: Option<String>,
    pub backdrop_visible: bool,
    pub backdrop_pending: bool,
    pub backdrop_failure: Option<String>,
    pub backdrop_captures: u64,
    pub backdrop_capture_ms: Vec<f64>,
    /// 截图 p95 超限后使用主题底色，避免反复发起缓慢的原生捕获。
    pub backdrop_slow_fallback: bool,
}

struct PreviewReceipt {
    request: backdrop::Request,
    capture: AppResult<platform::PreviewCapture>,
}

struct NativeSurface {
    webview: platform::NativeWebView,
    parent_id: u64,
    bounds: Option<PluginWebViewBounds>,
    clip: Option<PluginWebViewBounds>,
    scale_factor: f32,
    requested_visible: bool,
    host_occluded: bool,
    visible: bool,
    engine_visible: bool,
    failure: Option<String>,
    backdrop: backdrop::Backdrop,
    capture_samples_ms: Vec<f64>,
    backdrop_slow_fallback: bool,
    preview_sender: mpsc::SyncSender<PreviewReceipt>,
}

impl NativeSurface {
    /// 浮层期间先撤下原生画面和输入；正常截图保留引擎，延迟超限则彻底隐藏引擎。
    fn synchronize(&mut self, modal: bool, window: &mut Window) -> AppResult<()> {
        let overlay = self.requested_visible
            && (modal || self.host_occluded)
            && self.failure.is_none()
            && self.clip.is_some();
        let geometry = self
            .bounds
            .filter(|_| overlay && !self.backdrop_slow_fallback)
            .map(|bounds| backdrop::Geometry {
                bounds,
                scale: self.scale_factor,
            });
        if let Some(image) = self.backdrop.target(geometry) {
            let _ = window.drop_image(image);
        }
        let visible = self.requested_visible
            && !self.host_occluded
            && !modal
            && self.failure.is_none()
            && self.bounds.is_some()
            && self.clip.is_some();
        let engine_visible = if cfg!(any(windows, target_os = "linux", target_os = "macos")) {
            self.requested_visible
                && self.failure.is_none()
                && self.bounds.is_some()
                && self.clip.is_some()
                && !(overlay && self.backdrop_slow_fallback)
        } else {
            visible
        };
        if visible != self.visible || engine_visible != self.engine_visible {
            let focus_result = if !visible {
                platform::return_focus_to_parent(&self.webview, self.parent_id)
            } else {
                Ok(())
            };
            // 先隔离输入和子容器区域再显示引擎；任一步失败真正隐藏，不能仅更新逻辑状态。
            let transition = (|| {
                if !visible {
                    platform::set_interactive(&self.webview, false)?;
                }
                platform::set_clip(&self.webview, if visible { self.clip } else { None })?;
                if engine_visible != self.engine_visible {
                    self.webview
                        .set_visible(engine_visible)
                        .map_err(|_| AppError::infrastructure("系统 WebView 无法更新可见状态"))?;
                }
                if visible {
                    platform::set_interactive(&self.webview, true)?;
                }
                focus_result
            })();
            if let Err(error) = transition {
                let _ = self.webview.set_visible(false);
                self.engine_visible = false;
                self.visible = false;
                return Err(error);
            }
            self.engine_visible = engine_visible;
            self.visible = visible;
        }
        if let Some(request) = self.backdrop.begin(Instant::now()) {
            let sender = self.preview_sender.clone();
            let result = self.webview.capture_preview(move |capture| {
                let _ = sender.try_send(PreviewReceipt { request, capture });
            });
            if let Err(error) = result {
                self.backdrop.finish(request, Err(error));
            }
        }
        Ok(())
    }

    /// 布局只调整已创建的子窗口；创建、导航和消息订阅均不发生在渲染阶段。
    fn layout(&mut self, bounds: Bounds<Pixels>, window: &mut Window, app: &mut App) {
        let mask = window.content_mask().bounds;
        let clipped = bounds.intersect(&mask);
        let next = physical_bounds(bounds, window.scale_factor());
        let clip = physical_bounds(clipped, window.scale_factor())
            .zip(next)
            .map(|(clip, full)| PluginWebViewBounds {
                x: clip.x - full.x,
                y: clip.y - full.y,
                width: clip.width,
                height: clip.height,
            });
        if next != self.bounds || clip != self.clip {
            if let Some(next) = next {
                if let Err(error) = self.webview.set_bounds(wry::Rect {
                    position: wry::dpi::PhysicalPosition::new(next.x, next.y).into(),
                    size: wry::dpi::PhysicalSize::new(next.width, next.height).into(),
                }) {
                    self.failure = Some(format!("WebView 布局失败：{error}"));
                    let _ = self.webview.set_visible(false);
                    self.visible = false;
                    self.engine_visible = false;
                    return;
                }
                if let Err(error) =
                    platform::set_clip(&self.webview, if self.visible { clip } else { None })
                {
                    self.failure = Some(error.to_string());
                    let _ = self.webview.set_visible(false);
                    self.visible = false;
                    self.engine_visible = false;
                    return;
                }
            }
            self.bounds = next;
            self.clip = clip;
        }
        self.scale_factor = window.scale_factor();
        if let Err(error) = self.synchronize(native_overlay_visible(window, app), window) {
            self.failure = Some(error.to_string());
        }
    }
}

/// GPUI 实体拥有唯一系统子视图；隐藏不销毁，实体释放才取消泵并释放原生实例。
pub struct PluginWebView {
    _pump: Task<()>,
    _window_bounds_subscription: Subscription,
    _release_subscription: Subscription,
    _preview_decode: Option<Task<()>>,
    native: Rc<RefCell<NativeSurface>>,
    window_id: WindowId,
    native_parent_id: u64,
    instance_id: String,
    events: mpsc::Receiver<PluginWebViewEvent>,
    previews: mpsc::Receiver<PreviewReceipt>,
    loaded: bool,
    message_count: u64,
    reported_failure: Option<String>,
}

impl EventEmitter<PluginWebViewEvent> for PluginWebView {}

impl Drop for PluginWebView {
    /// 旧绘制帧可能短暂保留 Canvas 的 Rc；实体撤下即隐藏，不能等最后一帧释放才撤销原生输入。
    fn drop(&mut self) {
        let native = self.native.borrow_mut();
        let _ = platform::return_focus_to_parent(&native.webview, native.parent_id);
        let _ = native.webview.set_visible(false);
    }
}

impl PluginWebView {
    /// 两类文档共用原生生命周期；资源校验必须先于引擎创建，失败交回宿主恢复界面。
    pub fn create(
        document: impl Into<PluginWebViewSource>,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Entity<Self>> {
        let document = document.into();
        document.validate()?;
        let instance_id = document.instance_id().to_owned();
        let native_parent_id = platform::parent_id(window)?;
        let (sender, events) = mpsc::sync_channel(MAX_QUEUED_EVENTS);
        let (preview_sender, previews) = mpsc::sync_channel(1);
        let webview = platform::create(document, sender, window)?;
        let native = Rc::new(RefCell::new(NativeSurface {
            webview,
            parent_id: native_parent_id,
            bounds: None,
            clip: None,
            scale_factor: window.scale_factor(),
            requested_visible: true,
            host_occluded: false,
            visible: false,
            engine_visible: false,
            failure: None,
            backdrop: backdrop::Backdrop::default(),
            capture_samples_ms: Vec::new(),
            backdrop_slow_fallback: false,
            preview_sender,
        }));
        let view = cx.new(|cx: &mut Context<Self>| {
            let window_bounds_subscription =
                cx.observe_window_bounds(window, Self::parent_window_bounds_changed);
            let owner_window = window.window_handle();
            let release_subscription = cx.on_release(move |view, app| {
                if let Some(image) = view.native.borrow_mut().backdrop.target(None) {
                    // 延后到窗口借用结束；关闭窗口本身则会释放整个atlas，不借机创建新的窗口。
                    app.defer(move |app| {
                        let _ = owner_window.update(app, |_, window, _| window.drop_image(image));
                    });
                }
            });
            let pump = cx.spawn_in(window, async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(16))
                        .await;
                    // GTK 回调不能在持有 GPUI 实体或 native RefCell 借用时重入。
                    platform::pump_events();
                    let alive =
                        cx.update(|window, app| this.update(app, |view, cx| view.pump(window, cx)));
                    if !matches!(alive, Ok(Ok(()))) {
                        break;
                    }
                }
            });
            Self {
                _pump: pump,
                _window_bounds_subscription: window_bounds_subscription,
                _release_subscription: release_subscription,
                _preview_decode: None,
                native,
                window_id: window.window_handle().window_id(),
                native_parent_id,
                instance_id,
                events,
                previews,
                loaded: false,
                message_count: 0,
                reported_failure: None,
            }
        });
        occlusion::register(&view, window, cx)?;
        Ok(view)
    }

    /// Root 在设置、命令面板等自绘覆盖层改变时传入本窗口合并的遮挡状态。
    pub fn set_window_occluded(window: &mut Window, cx: &mut App, blocked: bool) -> AppResult<()> {
        occlusion::set_window_occluded(window, cx, blocked)
    }

    /// 仅宿主自己投影的前景抽屉可豁免其内部网页，页面不能自行申请越过模态。
    pub(crate) fn set_window_occluded_except(
        window: &mut Window,
        cx: &mut App,
        blocked: bool,
        foreground: Option<String>,
    ) -> AppResult<()> {
        occlusion::set_window_occluded_except(window, cx, blocked, foreground)
    }

    /// Tab/工具生命周期必须显式同步；隐藏保留 DOM、草稿、滚动和同一原生实例。
    pub fn set_visible(
        &mut self,
        visible: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        self.check_window(window)?;
        self.native.borrow_mut().requested_visible = visible;
        self.native
            .borrow_mut()
            .synchronize(native_overlay_visible(window, cx), window)?;
        cx.notify();
        Ok(())
    }

    /// 自绘设置、命令面板等非 Kit 模态由宿主显式遮挡；关闭后仍恢复原子视图。
    pub fn set_occluded(
        &mut self,
        occluded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        self.check_window(window)?;
        self.native.borrow_mut().host_occluded = occluded;
        self.native
            .borrow_mut()
            .synchronize(native_overlay_visible(window, cx), window)?;
        cx.notify();
        Ok(())
    }

    /// 外发数据只以 JSON 字面量进入固定脚本，插件不能把回执内容变成宿主注入的代码。
    pub fn post_message(&self, message: &Value) -> AppResult<()> {
        let encoded = serde_json::to_string(message)
            .map_err(|_| AppError::invalid("WebView 回执无法编码"))?;
        if encoded.len() > MAX_BRIDGE_BYTES {
            return Err(AppError::invalid("WebView 回执超过大小限制"));
        }
        self.native
            .borrow()
            .webview
            .evaluate_script(&format!(
                "window.dispatchEvent(new CustomEvent('kerminal:message',{{detail:{encoded}}}));"
            ))
            .map_err(|_| AppError::infrastructure("系统 WebView 无法接收宿主回执"))
    }

    /// 仅隔离验收构建可注入探针脚本；正式网页仍只能使用受验消息桥接。
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn evaluate_acceptance_script(&self, script: &str) -> AppResult<()> {
        self.native
            .borrow()
            .webview
            .evaluate_script(script)
            .map_err(|_| AppError::infrastructure("网页验收脚本执行失败"))
    }

    /// 焦点仅由显式用户导航转交，普通布局和恢复显示不会抢夺终端输入。
    pub fn focus(&self, window: &Window) -> AppResult<()> {
        self.check_window(window)?;
        let native = self.native.borrow();
        if !native.visible {
            return Err(AppError::conflict("插件 WebView 当前不可见"));
        }
        native
            .webview
            .focus()
            .map_err(|_| AppError::infrastructure("系统 WebView 无法获取焦点"))
    }

    /// 同时返回实例身份和物理区域，验收才能证明隐藏恢复没有重建或偏移。
    pub fn snapshot(&self) -> PluginWebViewSnapshot {
        let native = self.native.borrow();
        PluginWebViewSnapshot {
            instance_id: self.instance_id.clone(),
            native_id: native.webview.id().to_string(),
            native_parent_id: self.native_parent_id,
            visible: native.visible,
            engine_visible: native.engine_visible,
            loaded: self.loaded,
            bounds: native.bounds,
            clip: native.clip,
            scale_factor: native.scale_factor,
            message_count: self.message_count,
            parent_position_notifications: native.webview.parent_position_notifications(),
            failure: native.failure.clone(),
            backdrop_visible: !native.visible && native.backdrop.image.is_some(),
            backdrop_pending: native.backdrop.pending(),
            backdrop_failure: native.backdrop.failure.clone(),
            backdrop_captures: native.backdrop.captures,
            backdrop_capture_ms: native.capture_samples_ms.clone(),
            backdrop_slow_fallback: native.backdrop_slow_fallback,
        }
    }

    /// Wry 的 child 模式不订阅祖先移动；实体持有本窗口订阅，避免逐帧查询及悬挂原生回调。
    fn parent_window_bounds_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let result = self.check_window(window).and_then(|()| {
            self.native
                .borrow_mut()
                .webview
                .synchronize_parent_position()
        });
        if let Err(error) = result {
            let mut native = self.native.borrow_mut();
            native.failure = Some(error.to_string());
            let _ = native.synchronize(native_overlay_visible(window, cx), window);
            cx.notify();
        }
    }

    /// 回调每轮有界；原生循环继续处理在途回执，隐藏不会杀掉正在执行的插件操作。
    fn pump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut native = self.native.borrow_mut();
        let previous_backdrop_failure = native.backdrop.failure.clone();
        if let Err(error) = native.synchronize(native_overlay_visible(window, cx), window) {
            native.failure = Some(error.to_string());
        }
        let failure = native.failure.clone();
        let backdrop_changed = previous_backdrop_failure != native.backdrop.failure;
        drop(native);
        if backdrop_changed {
            cx.notify();
        }
        if let Ok(receipt) = self.previews.try_recv() {
            self.decode_preview(receipt, window, cx);
        }
        if failure != self.reported_failure {
            self.reported_failure = failure.clone();
            if let Some(message) = failure {
                cx.emit(PluginWebViewEvent::Failed {
                    instance_id: self.instance_id.clone(),
                    message,
                });
            }
        }
        for _ in 0..MAX_QUEUED_EVENTS {
            let Ok(event) = self.events.try_recv() else {
                break;
            };
            match &event {
                PluginWebViewEvent::Loaded { .. } => self.loaded = true,
                PluginWebViewEvent::Message { .. } => {
                    self.message_count = self.message_count.saturating_add(1)
                }
                PluginWebViewEvent::Failed { .. }
                | PluginWebViewEvent::ExternalNavigation { .. } => {}
            }
            cx.emit(event);
        }
    }

    /// 原生回调只搬运有界字节；PNG解码放在后台，结果回到原窗口后再次核对遮挡代次。
    fn decode_preview(
        &mut self,
        receipt: PreviewReceipt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = receipt.request;
        if !self.native.borrow().backdrop.accepts(request) {
            self.native
                .borrow_mut()
                .backdrop
                .finish(request, Err(AppError::conflict("网页预览已过期")));
            return;
        }
        let capture = match receipt.capture {
            Ok(capture) => capture,
            Err(error) => {
                self.native
                    .borrow_mut()
                    .backdrop
                    .finish(request, Err(error));
                cx.notify();
                return;
            }
        };
        let fallback = {
            let mut native = self.native.borrow_mut();
            if native.capture_samples_ms.len() < 256 {
                native
                    .capture_samples_ms
                    .push(capture.api_duration.as_secs_f64() * 1000.);
            }
            native.backdrop_slow_fallback = backdrop::slow_capture(&native.capture_samples_ms);
            if native.backdrop_slow_fallback {
                native.backdrop.finish(
                    request,
                    Err(AppError::infrastructure("网页预览延迟过高，已使用背景色")),
                );
            }
            native.backdrop_slow_fallback
        };
        if fallback {
            cx.notify();
            return;
        }
        let decode = cx
            .background_executor()
            .spawn(async move { backdrop::decode_capture(capture.data, request.geometry) });
        self._preview_decode = Some(cx.spawn_in(window, async move |this, cx| {
            let image = decode.await;
            let _ = this.update(cx, |view, cx| {
                if view.native.borrow_mut().backdrop.finish(request, image) {
                    cx.notify();
                }
            });
        }));
    }

    /// 原生子窗不能跨 GPUI 窗口迁移；调用方须在新窗口创建新的受管实例。
    fn check_window(&self, window: &Window) -> AppResult<()> {
        if self.window_id != window.window_handle().window_id() {
            return Err(AppError::conflict("插件 WebView 属于另一个窗口"));
        }
        Ok(())
    }
}

impl Render for PluginWebView {
    /// 背板只在原生输入撤下后绘制；等待或失败时沿用宿主底色，避免深色主题突然闪出白底。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::ThemeTokens::current(window, cx);
        let native = self.native.clone();
        let show_fallback = {
            let state = native.borrow();
            !state.visible
                && state.requested_visible
                && (state.backdrop_slow_fallback || state.backdrop.failure.is_some())
        };
        let painting = native.clone();
        div()
            .relative()
            .size_full()
            .bg(theme.background)
            .overflow_hidden()
            .child(
                canvas(
                    move |bounds, window, app| {
                        native.borrow_mut().layout(bounds, window, app);
                    },
                    move |bounds, (), window, _| {
                        let native = painting.borrow();
                        if !native.visible
                            && let Some(image) = &native.backdrop.image
                        {
                            let _ = window.paint_image(
                                bounds,
                                bounds,
                                Corners::default(),
                                Arc::clone(image),
                                0,
                                false,
                            );
                        }
                    },
                )
                .size_full(),
            )
            .when(show_fallback, |node| {
                node.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(gpui_kit::img(fallback_application_icon()).size(gpui_kit::px(32.))),
                )
            })
    }
}

/// 截图不可用时重用程序内置图标；共享图片身份避免每帧重复解码与分配。
fn fallback_application_icon() -> Arc<gpui_kit::Image> {
    static ICON: OnceLock<Arc<gpui_kit::Image>> = OnceLock::new();
    ICON.get_or_init(|| {
        Arc::new(gpui_kit::Image::from_bytes(
            gpui_kit::ImageFormat::Png,
            include_bytes!("../../../../resources/icons/32x32.png").to_vec(),
        ))
    })
    .clone()
}

/// 只检查本窗口浮层；通知保留到退出动画结束，随后恢复同一原生子视图。
fn native_overlay_visible(window: &mut Window, app: &mut App) -> bool {
    window.has_active_dialog(app)
        || window.has_active_prompt()
        || !window.notifications(app).is_empty()
        || gpui_kit::base::GlobalState::has_deferred_popover_in_window(
            window.window_handle().window_id(),
            app,
        )
}

/// 边缘分别四舍五入，窄窗与分数 DPI 下不会因宽度单独舍入产生一像素缝隙。
fn physical_bounds(bounds: Bounds<Pixels>, scale: f32) -> Option<PluginWebViewBounds> {
    let x = f32::from(bounds.origin.x) * scale;
    let y = f32::from(bounds.origin.y) * scale;
    let right = (f32::from(bounds.origin.x) + f32::from(bounds.size.width)) * scale;
    let bottom = (f32::from(bounds.origin.y) + f32::from(bounds.size.height)) * scale;
    if ![x, y, right, bottom, scale]
        .iter()
        .all(|value| value.is_finite())
        || scale <= 0.
        || right <= x
        || bottom <= y
    {
        return None;
    }
    let x = x.round() as i32;
    let y = y.round() as i32;
    let width = (right.round() as i32).checked_sub(x)? as u32;
    let height = (bottom.round() as i32).checked_sub(y)? as u32;
    (width > 0 && height > 0).then_some(PluginWebViewBounds {
        x,
        y,
        width,
        height,
    })
}

#[cfg(test)]
#[path = "../../tests/unit/plugin_webview.rs"]
mod tests;
