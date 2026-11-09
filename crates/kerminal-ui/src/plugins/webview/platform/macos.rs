//! WKWebView 使用独占容器裁剪输入与画面，截图只传递有界数据给后台解码。
//!
//! @author kongweiguang
//!
//! macOS 目标依赖需启用 objc2-web-kit 的 WKSnapshotConfiguration/block2、
//! objc2-app-kit 的 NSImage/NSView/NSWindow/objc2-quartz-core 及 image 的 tiff。

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Instant,
};

use block2::RcBlock;
use kerminal_application::{AppError, AppResult};
use objc2::{MainThreadMarker, MainThreadOnly, rc::Retained};
use objc2_app_kit::{NSImage, NSView};
use objc2_foundation::{NSError, NSPoint, NSRect, NSSize};
use wry::WebViewExtMacOS as _;

use super::{PluginWebViewBounds, PreviewCapture, PreviewData};

const MAX_PREVIEW_BYTES: usize = 32 * 1024 * 1024;

/// 专属容器限制命中区域和可见区域，网页本身保留完整尺寸以维持 DOM 视口。
pub(super) struct Surface {
    container: Retained<NSView>,
    bounds: Cell<wry::Rect>,
    clip: Cell<PluginWebViewBounds>,
}

impl Surface {
    /// 复用 Wry 创建的子视图，并将它移入唯一容器，避免修改共享父视图的裁剪属性。
    pub(super) fn new(webview: &wry::WebView) -> AppResult<Self> {
        let view = webview.webview();
        // SAFETY: Wry 和其父视图由同一主线程持有，建立容器期间父窗口仍存活。
        let parent = unsafe { view.superview() }
            .ok_or_else(|| AppError::infrastructure("WKWebView 父视图不可用"))?;
        let main = MainThreadMarker::new()
            .ok_or_else(|| AppError::infrastructure("WKWebView 必须在主线程创建"))?;
        let container = NSView::initWithFrame(
            NSView::alloc(main),
            NSRect::new(NSPoint::new(0., 0.), NSSize::new(0., 0.)),
        );
        parent.addSubview(&container);
        container.setWantsLayer(true);
        let Some(layer) = container.layer() else {
            container.removeFromSuperview();
            return Err(AppError::infrastructure("WKWebView 裁剪层不可用"));
        };
        layer.setMasksToBounds(true);
        view.removeFromSuperview();
        container.addSubview(&view);
        Ok(Self {
            container,
            bounds: Cell::new(wry::Rect {
                position: wry::dpi::PhysicalPosition::new(0, 0).into(),
                size: wry::dpi::PhysicalSize::new(1, 1).into(),
            }),
            clip: Cell::new(PluginWebViewBounds {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }),
        })
    }

    /// 完整尺寸与可见裁剪分开保存，滚动裁剪不能改变网页内的 CSS 布局。
    pub(super) fn set_bounds(&self, webview: &wry::WebView, bounds: wry::Rect) -> AppResult<()> {
        let previous = self.bounds.replace(bounds);
        if let Err(error) = self.layout(webview) {
            self.bounds.set(previous);
            return Err(error);
        }
        Ok(())
    }

    /// 零大小容器移除绘制与命中区域，但保留网页视图以接收截图回调。
    pub(super) fn set_clip(
        &self,
        webview: &wry::WebView,
        clip: PluginWebViewBounds,
    ) -> AppResult<()> {
        let previous = self.clip.replace(clip);
        if let Err(error) = self.layout(webview) {
            self.clip.set(previous);
            return Err(error);
        }
        Ok(())
    }

    /// 父窗口高度变化可能改变未翻转坐标系原点，即使网页自身像素区域未变也要重新定位。
    pub(super) fn parent_resized(&self, webview: &wry::WebView) -> AppResult<()> {
        self.layout(webview)
    }

    /// 按实际 backing scale 把物理像素还原为 AppKit 坐标，并处理父视图坐标方向。
    fn layout(&self, webview: &wry::WebView) -> AppResult<()> {
        let view = webview.webview();
        let window = view
            .window()
            .ok_or_else(|| AppError::infrastructure("WKWebView 窗口不可用"))?;
        // SAFETY: 容器只由本适配器持有并在主线程挂载，调用期间其父视图仍然有效。
        let parent = unsafe { self.container.superview() }
            .ok_or_else(|| AppError::infrastructure("WKWebView 容器已脱离父视图"))?;
        let scale = window.backingScaleFactor();
        if !scale.is_finite() || scale <= 0. {
            return Err(AppError::infrastructure("WKWebView 缩放系数无效"));
        }
        let full = self.bounds.get();
        let origin = full.position.to_logical::<f64>(scale);
        let size = full.size.to_logical::<f64>(scale);
        let clip = self.clip.get();
        let x = f64::from(clip.x) / scale;
        let y = f64::from(clip.y) / scale;
        let width = f64::from(clip.width) / scale;
        let height = f64::from(clip.height) / scale;
        let parent_y = if parent.isFlipped() {
            origin.y + y
        } else {
            parent.frame().size.height - origin.y - y - height
        };
        self.container.setFrame(NSRect::new(
            NSPoint::new(origin.x + x, parent_y),
            NSSize::new(width, height),
        ));
        let child_y = if self.container.isFlipped() {
            -y
        } else {
            height - size.height + y
        };
        view.setFrame(NSRect::new(
            NSPoint::new(-x, child_y),
            NSSize::new(size.width, size.height),
        ));
        Ok(())
    }
}

impl Drop for Surface {
    /// 容器由本实例独占，释放时从原父视图移除，避免残留透明输入层。
    fn drop(&mut self) {
        self.container.removeFromSuperview();
    }
}

/// 只在焦点仍属于该网页子树时归还宿主，避免关闭浮层抢走其他控件焦点。
pub(super) fn return_focus_to_parent(webview: &wry::WebView) -> AppResult<()> {
    let view = webview.webview();
    let Some(window) = view.window() else {
        return Err(AppError::infrastructure("WKWebView 焦点窗口不可用"));
    };
    if window
        .firstResponder()
        .and_then(|responder| {
            responder
                .downcast_ref::<NSView>()
                .map(|focused| focused.isDescendantOf(&view))
        })
        .unwrap_or(false)
    {
        webview
            .focus_parent()
            .map_err(|_| AppError::infrastructure("WKWebView 无法交还宿主焦点"))?;
    }
    Ok(())
}

/// WebKit 异步截图只在主线程获取 TIFF，解码与尺寸验证随后在后台完成。
pub(super) fn capture(
    webview: &wry::WebView,
    callback: impl FnOnce(AppResult<PreviewCapture>) + 'static,
) -> AppResult<()> {
    if MainThreadMarker::new().is_none() {
        return Err(AppError::infrastructure("WKWebView 截图必须在主线程发起"));
    }
    let started = Instant::now();
    let callback = Rc::new(RefCell::new(Some(callback)));
    let handler = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
        let Some(callback) = callback.borrow_mut().take() else {
            return;
        };
        let duration = started.elapsed();
        // SAFETY: 指针仅在 WebKit 完成回调期间借用，不跨线程或保存在回调之外。
        let image = unsafe { image.as_ref() };
        let result = if !error.is_null() {
            Err(AppError::infrastructure("WKWebView 无法生成页面截图"))
        } else if let Some(data) = image.and_then(NSImage::TIFFRepresentation) {
            if data.is_empty() || data.len() > MAX_PREVIEW_BYTES {
                Err(AppError::invalid("WKWebView 截图超过大小限制"))
            } else {
                Ok(PreviewCapture {
                    data: PreviewData::Tiff(data.to_vec()),
                    api_duration: duration,
                })
            }
        } else {
            Err(AppError::infrastructure("WKWebView 返回空截图"))
        };
        callback(result);
    });
    // SAFETY: WebKit 在主线程复制 completion block；闭包只借用回调传入的对象并持有自身状态。
    unsafe {
        webview
            .webview()
            .takeSnapshotWithConfiguration_completionHandler(None, &handler);
    }
    Ok(())
}
