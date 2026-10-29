//! 验证锁定 vendor 的滤镜顺序契约，避免依赖上游未打包的字体与测试宏资源。
//!
//! @author kongweiguang

#![cfg(target_os = "windows")]

use gpui_kit::{BackdropFilter, Bounds, ContentMask, Quad, ScaledPixels, Scene, point};

/// 样本邻域和可见区域使用不同矩形，捕获仅按可见边界排序导致的背景遗漏。
fn bounds(left: f32, top: f32, right: f32, bottom: f32) -> Bounds<ScaledPixels> {
    Bounds::from_corners(
        point(ScaledPixels(left), ScaledPixels(top)),
        point(ScaledPixels(right), ScaledPixels(bottom)),
    )
}

/// 滤镜必须位于采样邻域内容之后、活跃层后续文字之前，缓存 replay 也须保留相同屏障。
#[test]
fn backdrop_filter_preserves_sampling_support_active_layers_and_replay() {
    let clip_bounds = bounds(0., 0., 100., 100.);
    let mut scene = Scene::default();
    scene.insert_primitive(Quad {
        bounds: bounds(22., 40., 27., 50.),
        content_mask: ContentMask {
            bounds: clip_bounds,
        },
        ..Quad::default()
    });
    let filter = BackdropFilter {
        bounds: bounds(30., 30., 70., 80.),
        content_mask: ContentMask {
            bounds: clip_bounds,
        },
        blur_radius: 4.,
        ..BackdropFilter::default()
    };
    scene.insert_primitive(filter.clone());
    assert!(scene.backdrop_filters[0].order > scene.quads[0].order);

    scene.push_layer(clip_bounds);
    scene.insert_primitive(filter);
    scene.insert_primitive(Quad {
        bounds: bounds(35., 35., 65., 75.),
        content_mask: ContentMask {
            bounds: clip_bounds,
        },
        ..Quad::default()
    });
    assert!(scene.quads[1].order > scene.backdrop_filters[1].order);
    scene.pop_layer();

    let mut replayed = Scene::default();
    replayed.replay(0..scene.len(), &scene);
    assert!(replayed.backdrop_filters[0].order > replayed.quads[0].order);
    assert!(replayed.quads[1].order > replayed.backdrop_filters[1].order);
}
