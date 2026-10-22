//! 通过 GPUI 资源缓存异步绘制终端工作区背景，不在 UI 线程自行读取或解码图片。
//!
//! @author kongweiguang

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui_kit::{
    AnyElement, App, Bounds, Corners, IntoElement, ObjectFit, Pixels, RenderImage, Resource,
    SharedString, TextAlign, Window, canvas, div, fill, point, prelude::*, px, size,
};
use kerminal_application::preferences::{BackgroundFit, SettingsSnapshot};

use crate::theme::ThemeTokens;

const MAX_TILE_PAINTS: usize = 256;

/// 投影终端背景所需设置，避免绘制层依赖配置文件与完整偏好 DTO。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalBackgroundSettings {
    enabled: bool,
    image_path: Option<PathBuf>,
    fit: BackgroundFit,
}

impl TerminalBackgroundSettings {
    /// 从已加载的设置快照提取背景字段；空路径保持为空，不尝试猜测默认图片。
    ///
    /// @author kongweiguang
    pub(crate) fn from_snapshot(settings: &SettingsSnapshot) -> Self {
        let image_path = settings.background_image_path.trim();
        Self {
            enabled: settings.background_enabled,
            image_path: (!image_path.is_empty()).then(|| PathBuf::from(image_path)),
            fit: settings.background_fit,
        }
    }
}

/// 背景层与其状态提示分开装配，Root 可把状态提示放到透明工作区前景。
pub(crate) struct TerminalBackgroundLayers {
    pub(crate) underlay: AnyElement,
    pub(crate) status: Option<AnyElement>,
}

/// 从可选偏好快照构造完整窗口底层和独立状态层；未加载时只返回安全主题底色。
///
/// @author kongweiguang
pub(crate) fn render_layers(
    settings: Option<TerminalBackgroundSettings>,
    theme: ThemeTokens,
) -> TerminalBackgroundLayers {
    let underlay = render_underlay(settings.as_ref(), theme);
    let status = render_status(settings.as_ref(), theme);
    TerminalBackgroundLayers { underlay, status }
}

/// 每帧先绘制主题根底色，再按主题材料透明度叠加图片和遮罩，绝不把终端文字一并淡化。
///
/// @author kongweiguang
fn render_underlay(
    settings: Option<&TerminalBackgroundSettings>,
    theme: ThemeTokens,
) -> AnyElement {
    let mut root = div().size_full().relative().child(
        div()
            .absolute()
            .inset_0()
            .size_full()
            .bg(theme.materials.root_base),
    );
    let Some(settings) = settings.filter(|settings| settings.enabled) else {
        return root.into_any_element();
    };
    if !theme.materials.image_visible || theme.materials.image_opacity <= 0.0 {
        return root.into_any_element();
    }
    let Some(path) = settings.image_path.as_ref() else {
        return root.into_any_element();
    };

    let resource: Resource = path.clone().into();
    let image_resource = resource;
    let image_layer = match settings.fit {
        BackgroundFit::Cover | BackgroundFit::Contain => {
            let object_fit = if settings.fit == BackgroundFit::Cover {
                ObjectFit::Cover
            } else {
                ObjectFit::Contain
            };
            div()
                .absolute()
                .inset_0()
                .size_full()
                .child(
                    gpui_kit::img(path.clone())
                        .id("kerminal-terminal-background-image")
                        .size_full()
                        .object_fit(object_fit),
                )
                .into_any_element()
        }
        BackgroundFit::Tile => {
            let tile_path = path.clone();
            canvas(
                move |_, window, cx| {
                    window.use_asset::<gpui_kit::ImgResourceLoader>(&image_resource, cx)
                },
                move |bounds, loaded, window, cx| {
                    let Some(Ok(image)) = loaded else {
                        return;
                    };
                    window.with_global_id(
                        "kerminal-terminal-background-tile".into(),
                        |id, window| {
                            let reduce_motion = cx.reduce_motion();
                            let animation = window.with_element_state::<TileAnimationState, _>(
                                id,
                                |state, window| {
                                    let mut state = state.unwrap_or_default();
                                    let frame_count = image.frame_count();
                                    let current_frame =
                                        state.frame_index.min(frame_count.saturating_sub(1));
                                    let frame_delay = if frame_count == 0 {
                                        Duration::ZERO
                                    } else {
                                        Duration::from(image.delay(current_frame))
                                    };
                                    let frame = advance_tile_animation(
                                        &mut state,
                                        &tile_path,
                                        frame_count,
                                        frame_delay,
                                        Instant::now(),
                                        window.is_window_active(),
                                        reduce_motion,
                                    );
                                    (frame, state)
                                },
                            );
                            if animation.request_frame {
                                window.request_animation_frame();
                            }
                            paint_tile_background(bounds, image, animation.frame_index, window);
                        },
                    );
                },
            )
            .absolute()
            .inset_0()
            .size_full()
            .into_any_element()
        }
    };
    root = root.child(image_layer);
    if theme.materials.image_veil > 0.0 {
        root = root.child(
            div()
                .absolute()
                .inset_0()
                .size_full()
                .bg(theme.background.opacity(theme.materials.image_veil)),
        );
    }
    root.into_any_element()
}

/// 仅在用户打开背景功能时创建加载状态；普通工作区不会为未配置图片挂占位元素。
///
/// @author kongweiguang
fn render_status(
    settings: Option<&TerminalBackgroundSettings>,
    theme: ThemeTokens,
) -> Option<AnyElement> {
    let settings = settings.filter(|settings| settings.enabled)?;
    let Some(path) = settings.image_path.as_ref() else {
        return Some(
            render_status_card(
                crate::locale::text(
                    "已启用背景图片，但没有设置图片路径。",
                    "Background image is enabled, but no image path is set.",
                ),
                theme,
                false,
            )
            .into_any_element(),
        );
    };
    if !theme.materials.image_visible {
        return None;
    }

    let resource: Resource = path.clone().into();
    let status_resource = resource;
    Some(
        canvas(
            move |_, window, cx| {
                window.use_asset::<gpui_kit::ImgResourceLoader>(&status_resource, cx)
            },
            move |bounds, loaded, window, cx| match loaded {
                None => paint_status(
                    bounds,
                    crate::locale::text("正在加载背景图片…", "Loading background image…"),
                    theme,
                    false,
                    window,
                    cx,
                ),
                Some(Err(_)) => paint_status(
                    bounds,
                    crate::locale::text(
                        "背景图片无法读取，请检查文件路径和格式。",
                        "The background image could not be loaded. Check its path and format.",
                    ),
                    theme,
                    true,
                    window,
                    cx,
                ),
                Some(Ok(_)) => {}
            },
        )
        .absolute()
        .inset_0()
        .size_full()
        .into_any_element(),
    )
}

/// 在无图片时显示轻量提示；背景图片状态不应遮挡工作区或暗示设置已写入。
///
/// @author kongweiguang
fn render_status_card(message: SharedString, theme: ThemeTokens, is_error: bool) -> AnyElement {
    div()
        .size_full()
        .relative()
        .child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .child(status_badge(message, theme, is_error)),
        )
        .into_any_element()
}

/// 用一致的浅深主题表面承载异步状态文字，避免加载失败退化为无提示空白。
///
/// @author kongweiguang
fn status_badge(message: SharedString, theme: ThemeTokens, is_error: bool) -> impl IntoElement {
    let color = if is_error { theme.error } else { theme.muted };
    div()
        .px_3()
        .py_2()
        .bg(theme.panel.opacity(0.92))
        .border_1()
        .border_color(if is_error {
            theme.error
        } else {
            theme.muted.opacity(0.35)
        })
        .rounded_md()
        .text_color(color)
        .text_size(px(12.))
        .child(message)
}

/// 按 GPUI 已缓存的解码结果计算绘制位置；超小平铺图会受绘制上限保护。
///
/// @author kongweiguang
fn paint_tile_background(
    bounds: Bounds<Pixels>,
    image: Arc<RenderImage>,
    frame_index: usize,
    window: &mut Window,
) {
    if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) || image.frame_count() == 0 {
        return;
    }
    let frame_index = frame_index.min(image.frame_count() - 1);
    let image_size = image.size(frame_index);
    let image_width = u32::from(image_size.width) as f32;
    let image_height = u32::from(image_size.height) as f32;
    if image_width <= 0.0 || image_height <= 0.0 {
        return;
    }

    let Some(plan) = tile_plan(
        bounds.size.width.as_f32(),
        bounds.size.height.as_f32(),
        image_width,
        image_height,
    ) else {
        return;
    };
    for row in 0..plan.rows {
        for column in 0..plan.columns {
            let image_bounds = Bounds {
                origin: point(
                    bounds.origin.x + px(plan.tile_width * column as f32),
                    bounds.origin.y + px(plan.tile_height * row as f32),
                ),
                size: size(px(plan.tile_width), px(plan.tile_height)),
            };
            paint_one(bounds, image_bounds, image.clone(), frame_index, window);
        }
    }
}

/// 在工作区范围内裁剪一块平铺图，并用统一帧索引保持动画同步。
///
/// @author kongweiguang
fn paint_one(
    viewport: Bounds<Pixels>,
    image_bounds: Bounds<Pixels>,
    image: Arc<RenderImage>,
    frame_index: usize,
    window: &mut Window,
) {
    if let Err(error) = window.paint_image(
        viewport,
        image_bounds,
        Corners::default(),
        image,
        frame_index,
        false,
    ) {
        tracing::warn!(error = %error, "terminal background image paint failed");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AnimationFrame {
    frame_index: usize,
    request_frame: bool,
}

#[derive(Default)]
struct TileAnimationState {
    image_path: Option<PathBuf>,
    frame_index: usize,
    last_frame_at: Option<Instant>,
}

/// 用稳定图片路径保存平铺 GIF/WebP 的帧时钟，避免 resize 或重绘重复解码。
///
/// @author kongweiguang
fn advance_tile_animation(
    state: &mut TileAnimationState,
    image_path: &std::path::Path,
    frame_count: usize,
    frame_delay: Duration,
    now: Instant,
    window_active: bool,
    reduce_motion: bool,
) -> AnimationFrame {
    if state.image_path.as_deref() != Some(image_path) {
        state.image_path = Some(image_path.to_path_buf());
        state.frame_index = 0;
        state.last_frame_at = None;
    }

    if frame_count == 0 {
        state.frame_index = 0;
        state.last_frame_at = None;
        return AnimationFrame {
            frame_index: 0,
            request_frame: false,
        };
    }

    state.frame_index = state.frame_index.min(frame_count - 1);
    let request_frame = frame_count > 1 && window_active && !reduce_motion;
    if request_frame {
        if let Some(last_frame_at) = state.last_frame_at {
            let elapsed = now.saturating_duration_since(last_frame_at);
            if elapsed >= frame_delay {
                state.frame_index = (state.frame_index + 1) % frame_count;
                state.last_frame_at = Some(now - (elapsed - frame_delay));
            }
        } else {
            state.last_frame_at = Some(now);
        }
    } else {
        state.last_frame_at = None;
    }

    AnimationFrame {
        frame_index: state.frame_index,
        request_frame,
    }
}

/// 仅在图片未就绪或加载失败时绘制状态层，文字与图片透明度保持相互独立。
///
/// @author kongweiguang
fn paint_status(
    bounds: Bounds<Pixels>,
    message: SharedString,
    theme: ThemeTokens,
    is_error: bool,
    window: &mut Window,
    cx: &mut App,
) {
    if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
        return;
    }
    let color = if is_error { theme.error } else { theme.muted };
    let mut run = window.text_style().to_run(message.len());
    run.color = color;
    let line = window
        .text_system()
        .shape_line(message, px(13.), &[run], None);
    let card_size = size(
        (line.width() + px(28.)).min(bounds.size.width),
        px(40.).min(bounds.size.height),
    );
    let card_bounds = Bounds {
        origin: point(
            bounds.origin.x + (bounds.size.width - card_size.width) / 2.0,
            bounds.origin.y + (bounds.size.height - card_size.height) / 2.0,
        ),
        size: card_size,
    };
    window.paint_quad(fill(card_bounds, theme.panel.opacity(0.96)));
    let origin = point(
        card_bounds.origin.x + px(14.),
        card_bounds.origin.y + (card_bounds.size.height - px(18.)) / 2.0,
    );
    let _ = line.paint(origin, px(20.), TextAlign::Left, None, window, cx);
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TilePlan {
    tile_width: f32,
    tile_height: f32,
    columns: usize,
    rows: usize,
}

/// 限制超小图片的 draw-call 数量；必要时等比放大 tile，而不构造巨量 GPUI 子元素。
fn tile_plan(
    viewport_width: f32,
    viewport_height: f32,
    image_width: f32,
    image_height: f32,
) -> Option<TilePlan> {
    if ![viewport_width, viewport_height, image_width, image_height]
        .into_iter()
        .all(|value| value.is_finite() && value > 0.0)
    {
        return None;
    }

    let mut scale = 1.0;
    loop {
        let tile_width = image_width * scale;
        let tile_height = image_height * scale;
        let columns = (viewport_width / tile_width).ceil().max(1.0) as usize;
        let rows = (viewport_height / tile_height).ceil().max(1.0) as usize;
        if !tile_width.is_finite() || !tile_height.is_finite() {
            return None;
        }
        if columns.saturating_mul(rows) <= MAX_TILE_PAINTS {
            return Some(TilePlan {
                tile_width,
                tile_height,
                columns,
                rows,
            });
        }
        scale *= 1.1;
    }
}

#[cfg(test)]
#[path = "../tests/unit/terminal_background.rs"]
mod tests;
