//! @author kongweiguang

//! 浮层背板只持有一个真实网页帧；代次与单在途约束防止旧页面回执覆盖新的挂载位置。

use super::PluginWebViewBounds;
use gpui_kit::RenderImage;
use kerminal_application::{AppError, AppResult};
use std::{
    io::Cursor,
    sync::Arc,
    time::{Duration, Instant},
};

const MAX_PNG_BYTES: usize = 32 * 1024 * 1024;
const MAX_PIXELS: u64 = 8_294_400;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(3);
const MIN_LATENCY_SAMPLES: usize = 20;
const MAX_P95_MILLIS: f64 = 50.0;

/// 以至少二十次真实回执的最近秩 p95 决定降级，避免单次冷启动波动直接禁用截图。
pub(super) fn slow_capture(samples_ms: &[f64]) -> bool {
    if samples_ms.len() < MIN_LATENCY_SAMPLES {
        return false;
    }
    let mut ordered = samples_ms.to_vec();
    ordered.sort_unstable_by(f64::total_cmp);
    let rank = (ordered.len() * 95).div_ceil(100);
    ordered[rank - 1] > MAX_P95_MILLIS
}

/// 诊断仅携带视口尺寸；通过类型白名单输出数值，不展开任意插件或底层错误源。
#[derive(Debug)]
struct PreviewSizeMismatch {
    actual: (u32, u32),
    expected: (u32, u32),
}

impl std::fmt::Display for PreviewSizeMismatch {
    /// 仅尺寸差异可见于验收快照，PNG正文与磁盘路径始终留在诊断边界之外。
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            output,
            "网页预览尺寸已变化：{}x{}，预期{}x{}",
            self.actual.0, self.actual.1, self.expected.0, self.expected.1
        )
    }
}

impl std::error::Error for PreviewSizeMismatch {}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Geometry {
    pub bounds: PluginWebViewBounds,
    pub scale: f32,
}

#[derive(Clone, Copy)]
pub(super) struct Request {
    pub generation: u64,
    pub geometry: Geometry,
    started: Instant,
    expired: bool,
}

#[derive(Default)]
pub(super) struct Backdrop {
    geometry: Option<Geometry>,
    generation: u64,
    attempted: bool,
    pending: Option<Request>,
    pub image: Option<Arc<RenderImage>>,
    pub failure: Option<String>,
    pub captures: u64,
}

impl Backdrop {
    /// 仅浮层遮挡才保留画面；切Tab、恢复原生和改变尺寸立即撤销旧代并交还GPU缓存给窗口回收。
    pub fn target(&mut self, geometry: Option<Geometry>) -> Option<Arc<RenderImage>> {
        if self.geometry == geometry {
            return None;
        }
        self.geometry = geometry;
        self.generation = self.generation.wrapping_add(1);
        self.attempted = false;
        self.failure = None;
        self.image.take()
    }

    /// 超时不释放原生在途槽；迟迟不回调的引擎不能因反复开关菜单积累无限截图请求。
    pub fn begin(&mut self, now: Instant) -> Option<Request> {
        if let Some(pending) = self.pending.as_mut() {
            if now.saturating_duration_since(pending.started) >= CAPTURE_TIMEOUT {
                pending.expired = true;
                if pending.generation == self.generation {
                    self.failure = Some("页面预览等待超时".into());
                }
            }
            return None;
        }
        let geometry = self.geometry?;
        if self.attempted {
            return None;
        }
        self.attempted = true;
        if !valid_geometry(geometry) {
            self.failure = Some("页面尺寸超过预览限制".into());
            return None;
        }
        let request = Request {
            generation: self.generation,
            geometry,
            started: now,
            expired: false,
        };
        self.pending = Some(request);
        self.captures = self.captures.saturating_add(1);
        Some(request)
    }

    /// 结果只消费其原请求；旧代、超时及隐藏后的结果不成为画面，也不污染正常网页的可用状态。
    pub fn finish(&mut self, request: Request, image: AppResult<Arc<RenderImage>>) -> bool {
        let Some(pending) = self
            .pending
            .filter(|pending| pending.generation == request.generation)
        else {
            return false;
        };
        self.pending = None;
        if pending.expired
            || self.generation != request.generation
            || self.geometry != Some(request.geometry)
        {
            return false;
        }
        match image {
            Ok(image) => {
                self.image = Some(image);
                self.failure = None;
            }
            // 本管线的错误仅含固定阶段说明/数值，不回传PNG、路径或插件正文。
            Err(error) => {
                self.failure = Some(
                    std::error::Error::source(&error)
                        .and_then(|source| source.downcast_ref::<PreviewSizeMismatch>())
                        .map_or_else(|| error.to_string(), ToString::to_string),
                );
            }
        }
        true
    }

    /// PNG回调和后台解码共用同一个在途槽，避免解码期间另发截图叠加内存。
    pub fn accepts(&self, request: Request) -> bool {
        self.pending
            .is_some_and(|pending| !pending.expired && pending.generation == request.generation)
            && self.generation == request.generation
            && self.geometry == Some(request.geometry)
    }

    /// 验收只读状态，不暴露截图字节或插件正文到日志。
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
}

/// 先限制原生viewport再截图，避免4K以上的CPU/GPU副本不受控地占用宿主内存。
fn valid_geometry(geometry: Geometry) -> bool {
    let bounds = geometry.bounds;
    geometry.scale.is_finite()
        && geometry.scale > 0.
        && bounds.width > 0
        && bounds.height > 0
        && bounds.width <= 8192
        && bounds.height <= 8192
        && u64::from(bounds.width) * u64::from(bounds.height) <= MAX_PIXELS
}

/// 引擎把CSS视口向上取整后还原物理像素；只接受原尺寸或该精确量化结果，不允许任意尺寸容差。
fn preview_dimension_matches(actual: u32, physical: u32, scale: f32) -> bool {
    actual == physical
        || actual
            == ((f64::from(physical) / f64::from(scale)).ceil() * f64::from(scale)).round() as u32
}

/// 引擎回调只携带原始截图；PNG 编码与解码统一在后台进行并保留相同的预算门禁。
pub(super) fn decode_capture(
    capture: super::platform::PreviewData,
    geometry: Geometry,
) -> AppResult<Arc<RenderImage>> {
    match capture {
        super::platform::PreviewData::Png(bytes) => decode(bytes, geometry),
        #[cfg(target_os = "macos")]
        super::platform::PreviewData::Tiff(bytes) => {
            decode_with_format(bytes, geometry, image::ImageFormat::Tiff)
        }
        #[cfg(target_os = "linux")]
        super::platform::PreviewData::Cairo {
            data,
            width,
            height,
            stride,
            format,
        } => {
            let surface =
                gtk::cairo::ImageSurface::create_for_data(data, format, width, height, stride)
                    .map_err(|_| AppError::infrastructure("无法读取 WebKitGTK 截图像素"))?;
            let mut output = LimitedPng::default();
            surface
                .write_to_png(&mut output)
                .map_err(|_| AppError::infrastructure("无法编码 WebKitGTK 截图"))?;
            decode(output.bytes, geometry)
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct LimitedPng {
    bytes: Vec<u8>,
}

#[cfg(target_os = "linux")]
impl std::io::Write for LimitedPng {
    /// 编码器必须在增长前受到字节上限约束，失败时丢弃整张图而非返回截断内容。
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let size = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_PNG_BYTES)
            .ok_or_else(|| std::io::Error::other("网页预览超过大小限制"))?;
        self.bytes
            .try_reserve(size - self.bytes.len())
            .map_err(|_| std::io::Error::other("网页预览内存分配失败"))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// 输出只存在内存中，无需额外刷新或同步磁盘。
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 解码在后台执行；先验证本代视口的DPI量化与分配预算，再将RGBA转为GPUI的BGRA。
pub(super) fn decode(bytes: Vec<u8>, geometry: Geometry) -> AppResult<Arc<RenderImage>> {
    decode_with_format(bytes, geometry, image::ImageFormat::Png)
}

/// 各系统编码使用同一尺寸与内存预算，格式仅来自宿主截图类型而非页面输入。
fn decode_with_format(
    bytes: Vec<u8>,
    geometry: Geometry,
    format: image::ImageFormat,
) -> AppResult<Arc<RenderImage>> {
    if bytes.len() > MAX_PNG_BYTES || !valid_geometry(geometry) {
        return Err(AppError::invalid("网页预览超过大小限制"));
    }
    let dimensions = image::ImageReader::with_format(Cursor::new(&bytes), format)
        .into_dimensions()
        .map_err(|_| AppError::invalid("网页预览格式无效"))?;
    if dimensions.0 == 0
        || dimensions.1 == 0
        || dimensions.0 > 8192
        || dimensions.1 > 8192
        || u64::from(dimensions.0) * u64::from(dimensions.1) > MAX_PIXELS
    {
        return Err(AppError::invalid("网页预览超过大小限制"));
    }
    if !preview_dimension_matches(dimensions.0, geometry.bounds.width, geometry.scale)
        || !preview_dimension_matches(dimensions.1, geometry.bounds.height, geometry.scale)
    {
        return Err(AppError::infrastructure_with_source(
            "网页预览尺寸已变化",
            PreviewSizeMismatch {
                actual: dimensions,
                expected: (geometry.bounds.width, geometry.bounds.height),
            },
        ));
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(MAX_PIXELS * 8);
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let mut rgba = reader
        .decode()
        .map_err(|_| AppError::invalid("网页预览无法解码"))?
        .into_rgba8();
    for pixel in rgba.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])))
}

#[cfg(test)]
#[path = "../../../tests/unit/plugin_webview_backdrop.rs"]
mod tests;
