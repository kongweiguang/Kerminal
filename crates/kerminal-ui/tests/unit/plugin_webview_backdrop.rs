//! @author kongweiguang

use super::*;
use image::ImageEncoder as _;

/// 小图能验证真实PNG解码与色序，不需要原生窗口或大块测试内存。
fn geometry(width: u32, height: u32) -> Geometry {
    Geometry {
        bounds: PluginWebViewBounds {
            x: 0,
            y: 0,
            width,
            height,
        },
        scale: 1.,
    }
}

/// PNG由编码器生成，避免用无效夹具误把解析失败当成尺寸保护生效。
fn png() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&[11, 22, 33, 255], 1, 1, image::ExtendedColorType::Rgba8)
        .unwrap();
    bytes
}

/// 原生请求超时后仍占槽；反复关闭重开不能累积未结束的截图或把迟到图用在新一轮菜单。
#[test]
fn timed_out_capture_stays_single_flight_until_real_receipt() {
    let mut state = Backdrop::default();
    let now = Instant::now();
    state.target(Some(geometry(1, 1)));
    let old = state.begin(now).unwrap();
    assert!(state.begin(now + CAPTURE_TIMEOUT).is_none());
    assert!(state.pending());
    state.target(None);
    state.target(Some(geometry(1, 1)));
    assert!(state.begin(now + CAPTURE_TIMEOUT).is_none());
    assert!(!state.finish(old, decode(png(), old.geometry)));
    assert!(state.image.is_none());
    let current = state.begin(now + CAPTURE_TIMEOUT).unwrap();
    assert!(state.finish(current, decode(png(), current.geometry)));
    assert_eq!(state.captures, 2);
}

/// 尺寸/DPI变化和Tab隐藏撤销旧图，实体仍可在下一次真实浮层周期重新抓取。
#[test]
fn resize_hide_and_late_decode_keep_the_current_overlay_owner() {
    let mut state = Backdrop::default();
    state.target(Some(geometry(1, 1)));
    let old = state.begin(Instant::now()).unwrap();
    let mut moved = geometry(1, 1);
    moved.scale = 1.25;
    state.target(Some(moved));
    assert!(!state.accepts(old));
    assert!(!state.finish(old, decode(png(), old.geometry)));
    let next = state.begin(Instant::now()).unwrap();
    assert!(state.finish(next, decode(png(), moved)));
    let image = state
        .target(None)
        .expect("host must evict the returned atlas image");
    assert_eq!(image.as_bytes(0).unwrap(), &[33, 22, 11, 255]);
    assert!(state.image.is_none());
    assert!(state.begin(Instant::now()).is_none());
}

/// 捕获失败只降级本轮背板，不循环重试，也不能永久禁用原网页或后续菜单。
#[test]
fn preview_failure_does_not_poison_future_visibility() {
    let mut state = Backdrop::default();
    state.target(Some(geometry(1, 1)));
    let request = state.begin(Instant::now()).unwrap();
    assert!(state.finish(request, Err(AppError::infrastructure("capture failed"))));
    assert!(state.failure.is_some());
    assert!(state.begin(Instant::now()).is_none());
    state.target(None);
    assert!(state.failure.is_none());
    state.target(Some(geometry(1, 1)));
    assert!(state.begin(Instant::now()).is_some());
}

/// 使用最近秩区分阈值和异常值，同时要求足够回执才永久转为背景色。
#[test]
fn p95_falls_back_only_after_sufficient_slow_captures() {
    assert!(!slow_capture(&[100.0; 19]));
    assert!(!slow_capture(&[50.0; 20]));
    let mut samples = [10.0; 20];
    samples[0] = 100.0;
    assert!(!slow_capture(&samples));
    samples[1] = 50.1;
    assert!(slow_capture(&samples));
}

/// 维度先于PNG分配校验，旧帧不可拉伸成当前页面；合法像素转换为GPUI要求的BGRA。
#[test]
fn png_decode_rejects_bad_dimensions_and_preserves_color_order() {
    assert!(decode(png(), geometry(2, 1)).is_err());
    assert!(decode(vec![0; 20], geometry(1, 1)).is_err());
    assert!(decode(png(), geometry(8193, 1)).is_err());
    assert!(decode(png(), geometry(4096, 4096)).is_err());
    assert_eq!(
        decode(png(), geometry(1, 1)).unwrap().as_bytes(0).unwrap(),
        &[33, 22, 11, 255]
    );
}

/// 150%真机返回930x566而宿主为930x565；允许CSS量化，仍拒绝其它DPI或旧视口的图片。
#[test]
fn measured_fractional_dpi_preview_preserves_geometry_validation() {
    // 浮层中缩窄窗口实测806物理像素得到807，证明确为CSS向上取整而非四舍五入。
    assert!(preview_dimension_matches(807, 806, 1.5));
    assert!(!preview_dimension_matches(809, 806, 1.5));
    assert!(!preview_dimension_matches(807, 806, 1.));
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(
            &vec![127; 930 * 566 * 4],
            930,
            566,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
    let mut viewport = geometry(930, 565);
    viewport.scale = 1.5;
    assert!(decode(bytes.clone(), viewport).is_ok());
    viewport.scale = 1.;
    assert!(decode(bytes.clone(), viewport).is_err());
    viewport.scale = 1.25;
    assert!(decode(bytes.clone(), viewport).is_err());
    viewport.scale = 1.5;
    viewport.bounds.width = 934;
    assert!(decode(bytes, viewport).is_err());
}
