//! WebKitGTK 截图保持在相同主线程发起，像素所有权移交后台编码。
//!
//! @author kongweiguang

use std::time::Instant;

use kerminal_application::{AppError, AppResult};
use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt as _};
use wry::WebViewExtUnix as _;

use super::{PreviewCapture, PreviewData};

const MAX_PREVIEW_BYTES: usize = 32 * 1024 * 1024;

/// 只抓可见视口；回调记录 API 耗时，PNG 编码和图片解码都留给后台执行。
pub(super) fn capture(
    webview: &wry::WebView,
    callback: impl FnOnce(AppResult<PreviewCapture>) + 'static,
) -> AppResult<()> {
    let started = Instant::now();
    webview.webview().snapshot(
        SnapshotRegion::Visible,
        SnapshotOptions::NONE,
        None::<&gtk::gio::Cancellable>,
        move |result| {
            let elapsed = started.elapsed();
            let result = result
                .map_err(|_| AppError::infrastructure("WebKitGTK 无法生成页面截图"))
                .and_then(|surface| prepare_surface(surface, elapsed));
            callback(result);
        },
    );
    Ok(())
}

/// 只转移独占且有界的图像缓冲，避免在 GTK 回调中编码或跨线程访问活跃控件。
fn prepare_surface(
    surface: gtk::cairo::Surface,
    api_duration: std::time::Duration,
) -> AppResult<PreviewCapture> {
    let surface = gtk::cairo::ImageSurface::try_from(surface)
        .map_err(|_| AppError::infrastructure("WebKitGTK 截图不是图像表面"))?;
    let width = surface.width();
    let height = surface.height();
    let stride = surface.stride();
    let format = surface.format();
    let bytes = usize::try_from(stride).ok().and_then(|stride| {
        usize::try_from(height)
            .ok()
            .and_then(|height| stride.checked_mul(height))
    });
    if width <= 0 || height <= 0 || bytes.is_none_or(|bytes| bytes > MAX_PREVIEW_BYTES) {
        return Err(AppError::invalid("WebKitGTK 截图超过大小限制"));
    }
    let data = surface
        .take_data()
        .map_err(|_| AppError::infrastructure("WebKitGTK 截图像素仍被占用"))?;
    Ok(PreviewCapture {
        data: PreviewData::Cairo {
            data,
            width,
            height,
            stride,
            format,
        },
        api_duration,
    })
}
