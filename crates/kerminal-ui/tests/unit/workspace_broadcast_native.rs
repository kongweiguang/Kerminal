//! 生产广播组件的隔离原生布局与焦点验收，不创建实际终端或执行命令。
//!
//! @author kongweiguang

use super::*;
use gpui_kit::{
    AnyWindowHandle, AsyncApp, Context, Entity, InputEvent as _, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Render, Subscription, Window, WindowOptions, component::TitleBar,
    px, size,
};
use kerminal_application::preferences::ThemeMode;
use serde::Serialize;
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

const EDGE_EPSILON: f64 = 0.01;

struct BroadcastNativeHarness {
    bar: Entity<WorkspaceBroadcastBar>,
    _subscription: Subscription,
}

impl Render for BroadcastNativeHarness {
    /// 占位分屏只提供真实布局约束，截图不能被解释为已验证终端会话或命令执行。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.panel)
            .text_color(theme.foreground)
            .child(div().w_full().flex_none().child(self.bar.clone()))
            .child(
                div()
                    .id("broadcast-native-placeholder-pane")
                    .debug_selector(|| "broadcast-native-placeholder-pane".to_owned())
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .gap_2()
                    .p_2()
                    .children(["分屏 1 · 布局验收", "分屏 2 · 布局验收"].into_iter().map(
                        |title| {
                            div()
                                .flex_1()
                                .min_w_0()
                                .border_1()
                                .border_color(theme.border)
                                .p_3()
                                .child(title)
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted)
                                        .child("未连接真实终端"),
                                )
                        },
                    )),
            )
    }
}

#[derive(Clone, Serialize)]
struct NativeFrame {
    theme: String,
    width: u32,
    surface: String,
    file: String,
}

/// 显式 ignored 入口只供私有桌面 runner 调用，避免普通测试创建原生窗口或接触用户配置。
///
/// @author kongweiguang
#[test]
#[ignore = "requires the isolated Windows native broadcast runner"]
fn native_broadcast_component_acceptance() {
    let output = env::var_os("KERMINAL_BROADCAST_NATIVE_OUTPUT")
        .map(PathBuf::from)
        .expect("runner must set KERMINAL_BROADCAST_NATIVE_OUTPUT");
    fs::create_dir_all(&output).expect("create native acceptance output directory");
    let report_path = output.join("native-report.json");
    if report_path.exists() {
        fs::remove_file(&report_path).expect("remove stale native acceptance report");
    }

    let failure = Arc::new(Mutex::new(None::<String>));
    let frames = Arc::new(Mutex::new(Vec::<NativeFrame>::new()));
    let failure_for_app = Arc::clone(&failure);
    let frames_for_app = Arc::clone(&frames);

    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            crate::init(cx);
            let window_options = WindowOptions {
                show: false,
                focus: false,
                ..TitleBar::window_options()
            };
            let result = gpui_kit::open_window(window_options, cx, |window, cx| {
                let bar = cx.new(|cx| WorkspaceBroadcastBar::new(window, cx));
                let events = Arc::new(Mutex::new(Vec::new()));
                let recorded_events = Arc::clone(&events);
                let subscription = cx.subscribe(&bar, move |_, event, _| {
                    recorded_events
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(event.clone());
                });
                let harness = cx.new(|_| BroadcastNativeHarness {
                    bar: bar.clone(),
                    _subscription: subscription,
                });
                let window_handle = window.window_handle();
                let task_failure = Arc::clone(&failure_for_app);
                let task_frames = Arc::clone(&frames_for_app);
                cx.spawn(async move |cx| {
                    let result = run_native_acceptance(
                        cx,
                        window_handle,
                        bar,
                        events,
                        task_frames,
                        output.clone(),
                    )
                    .await;
                    if let Err(error) = result {
                        record_failure(&task_failure, error.to_string());
                    }
                    let _ = window_handle.update(cx, |_, window, _| window.remove_window());
                    cx.update(|cx| cx.quit());
                })
                .detach();
                harness
            });
            if let Err(error) = result {
                record_failure(&failure_for_app, error.to_string());
                cx.quit();
            }
        });

    if let Some(error) = failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        panic!("native broadcast acceptance failed: {error}");
    }

    let frames = frames
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert_eq!(frames.len(), 18, "three surfaces for each theme and width");
    let report = serde_json::json!({ "status": "passed", "frames": frames });
    fs::write(
        report_path,
        serde_json::to_vec_pretty(&report).expect("serialize native report"),
    )
    .expect("write native acceptance report");
}

/// 每轮重建相同的展示投影，目标标识只服务组件验收，不绑定真实会话。
///
/// @author kongweiguang
fn base_projection() -> WorkspaceBroadcastBarProjection {
    WorkspaceBroadcastBarProjection {
        tab_id: Some(kerminal_domain::identity::TabId::new()),
        pane_count: 2,
        selected_count: 2,
        targets: vec![
            target("pane-one", "本地分屏（布局样本）", true),
            target("pane-two", "另一分屏（布局样本）", false),
        ],
        draft: "echo 示例命令".to_owned(),
        ..WorkspaceBroadcastBarProjection::default()
    }
}

/// 使用稳定目标身份让菜单布局可重复观察，不连接本地或远端资源。
///
/// @author kongweiguang
fn target(id: &str, title: &str, current: bool) -> WorkspaceBroadcastTargetProjection {
    WorkspaceBroadcastTargetProjection {
        pane_id: kerminal_domain::identity::PaneId::parse(id).expect("valid placeholder pane id"),
        title: title.to_owned(),
        detail: "布局样本".to_owned(),
        protocol: "Local".to_owned(),
        selected: true,
        enabled: true,
        current,
    }
}

/// 串行切换主题与窗口大小，并等待平台尺寸事件，确保截图与断言使用同一原生场景。
///
/// @author kongweiguang
async fn run_native_acceptance(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    bar: Entity<WorkspaceBroadcastBar>,
    events: Arc<Mutex<Vec<WorkspaceBroadcastBarEvent>>>,
    frames: Arc<Mutex<Vec<NativeFrame>>>,
    output: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let tab_id = base_projection().tab_id.expect("test tab id");
    let themes = [
        (ThemeMode::Light, "light"),
        (ThemeMode::Dark, "dark"),
        (ThemeMode::System, "system"),
    ];

    for (theme, theme_name) in themes {
        for width in [900_u32, 480_u32] {
            let mut projection = base_projection();
            projection.tab_id = Some(tab_id.clone());
            window.update(cx, |_, window, _| {
                window.resize(size(px(width as f32), px(400.)));
            })?;
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            window.update(cx, |_, window, app| {
                bar.update(app, |bar, cx| {
                    crate::theme::apply_theme_mode(theme, window.appearance(), cx);
                    bar.set_projection(projection.clone(), window, cx);
                });
                draw_frame(window, app);
                assert_toolbar_bounds(window, width)?;
                save_frame(
                    window,
                    &output,
                    &mut frames.lock().unwrap(),
                    theme_name,
                    width,
                    "toolbar",
                )?;
                Ok::<_, io::Error>(())
            })??;

            if theme_name == "light" && width == 900 {
                focus_then_click_sync(cx, window, bar.clone(), Arc::clone(&events), &tab_id)
                    .await?;
            }

            click_selector(cx, window, "workspace-broadcast-mode-trigger")?;
            cx.background_executor()
                .timer(Duration::from_millis(60))
                .await;
            window.update(cx, |_, window, app| {
                draw_frame(window, app);
                assert_toolbar_bounds(window, width)?;
                let option = require_bounds(window, "workspace-broadcast-mode-option-All")?;
                assert_inside_viewport(window, option, "target popover option")?;
                save_frame(
                    window,
                    &output,
                    &mut frames.lock().unwrap(),
                    theme_name,
                    width,
                    "popover",
                )?;
                Ok::<_, io::Error>(())
            })??;
            click_selector(cx, window, "workspace-broadcast-mode-trigger")?;

            projection.result_summary = Some("已发送 0/64；64 个目标尚未发送。".to_owned());
            projection.result_details = (0..64)
                .map(|index| WorkspaceBroadcastDeliveryProjection {
                    title: format!("placeholder-target-{index:02}"),
                    state: "尚未发送".to_owned(),
                })
                .collect();
            click_selector(cx, window, "workspace-broadcast-mode-trigger")?;
            cx.background_executor()
                .timer(Duration::from_millis(60))
                .await;
            window.update(cx, |_, window, app| {
                bar.update(app, |bar, cx| {
                    bar.set_projection(projection.clone(), window, cx)
                });
                draw_frame(window, app);
                assert_toolbar_bounds(window, width)?;
                let details = require_bounds(window, "workspace-broadcast-deliveries")?;
                if details.size.height.to_f64() > 96. + EDGE_EPSILON {
                    return Err(io::Error::other(format!(
                        "delivery list exceeded 96 logical px: {}",
                        details.size.height.to_f64()
                    )));
                }
                assert_pane_visible(window)?;
                save_frame(
                    window,
                    &output,
                    &mut frames.lock().unwrap(),
                    theme_name,
                    width,
                    "delivery-errors",
                )?;
                Ok::<_, io::Error>(())
            })??;
            click_selector(cx, window, "workspace-broadcast-mode-trigger")?;
        }
    }
    Ok(())
}

/// 使用窗口内部事件验证同步开关的焦点与单次意图，不向操作系统注入键鼠。
///
/// @author kongweiguang
async fn focus_then_click_sync(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    bar: Entity<WorkspaceBroadcastBar>,
    events: Arc<Mutex<Vec<WorkspaceBroadcastBarEvent>>>,
    tab_id: &kerminal_domain::identity::TabId,
) -> Result<(), Box<dyn std::error::Error>> {
    window.update(cx, |_, window, app| {
        bar.update(app, |bar, cx| bar.focus_draft(window, cx));
        draw_frame(window, app);
    })?;
    let before = window.update(cx, |_, window, app| {
        window
            .focused(app)
            .expect("draft input must own native GPUI focus")
    })?;
    click_selector(cx, window, "workspace-broadcast-sync-toggle")?;
    cx.background_executor()
        .timer(Duration::from_millis(40))
        .await;
    let after = window.update(cx, |_, window, app| window.focused(app))?;
    assert_eq!(after, Some(before), "sync click must preserve draft focus");

    let emitted = events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert_eq!(
        emitted,
        vec![WorkspaceBroadcastBarEvent::ToggleSync {
            tab_id: tab_id.clone()
        }],
        "one native sync click must emit exactly one ToggleSync event"
    );
    Ok(())
}

/// 只命中当前帧可见控件，分数 DPI 仅容纳浮点舍入误差，避免错误坐标通过验收。
///
/// @author kongweiguang
fn click_selector(
    cx: &mut AsyncApp,
    window_handle: AnyWindowHandle,
    selector: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    window_handle.update(cx, |_, window, app| {
        let bounds = require_bounds(window, selector)?;
        assert_inside_viewport(window, bounds, selector)?;
        let position = bounds.center();
        for event in [
            MouseMoveEvent {
                position,
                ..MouseMoveEvent::default()
            }
            .to_platform_input(),
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..MouseDownEvent::default()
            }
            .to_platform_input(),
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..MouseUpEvent::default()
            }
            .to_platform_input(),
        ] {
            let _ = window.dispatch_event(event, app);
        }
        draw_frame(window, app);
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 主行必须为命令输入和右端动作保留独立空间，并保留可见分屏区域。
///
/// @author kongweiguang
fn assert_toolbar_bounds(window: &Window, width: u32) -> Result<(), io::Error> {
    let input = require_bounds(window, "workspace-broadcast-draft-input-slot")?;
    let actions = require_bounds(window, "workspace-broadcast-actions")?;
    if input.size.width.to_f64() < 80. {
        return Err(io::Error::other("draft input became too narrow"));
    }
    if input.right().to_f64() > actions.left().to_f64() + EDGE_EPSILON {
        return Err(io::Error::other("draft input overlaps right-side actions"));
    }
    if actions.right().to_f64() > width as f64 + EDGE_EPSILON {
        return Err(io::Error::other(
            "right-side actions exceed the requested window width",
        ));
    }
    assert_inside_viewport(window, actions, "right-side actions")?;
    assert_pane_visible(window)
}

/// 缺失真实帧边界立即失败，避免把未渲染或被裁剪的控件误判为可用。
///
/// @author kongweiguang
fn require_bounds(
    window: &Window,
    selector: &str,
) -> Result<gpui_kit::Bounds<gpui_kit::Pixels>, io::Error> {
    window
        .debug_selector_bounds(selector)
        .ok_or_else(|| io::Error::other(format!("missing visible selector {selector:?}")))
}

/// 逐边比较可见边界，容差只用于分数 DPI 的计算舍入。
///
/// @author kongweiguang
fn assert_inside_viewport(
    window: &Window,
    bounds: gpui_kit::Bounds<gpui_kit::Pixels>,
    name: &str,
) -> Result<(), io::Error> {
    let viewport = window.visual_viewport_bounds();
    if bounds.left().to_f64() + EDGE_EPSILON < viewport.left().to_f64()
        || bounds.top().to_f64() + EDGE_EPSILON < viewport.top().to_f64()
        || bounds.right().to_f64() > viewport.right().to_f64() + EDGE_EPSILON
        || bounds.bottom().to_f64() > viewport.bottom().to_f64() + EDGE_EPSILON
    {
        return Err(io::Error::other(format!(
            "{name} is outside the visible viewport"
        )));
    }
    Ok(())
}

/// 反馈出现后仍须保留完整可见的工作区，不能只验证一个被推到窗口外的最小高度。
///
/// @author kongweiguang
fn assert_pane_visible(window: &Window) -> Result<(), io::Error> {
    let pane = require_bounds(window, "broadcast-native-placeholder-pane")?;
    if pane.size.height.to_f64() < 80. {
        return Err(io::Error::other(
            "placeholder pane has no visible workspace area",
        ));
    }
    assert_inside_viewport(window, pane, "placeholder pane")
}

/// 边界读取和截图都以同一已绘制帧为准，避免断言落在旧场景。
///
/// @author kongweiguang
fn draw_frame(window: &mut Window, app: &mut gpui_kit::App) {
    let clear = window.draw(app);
    clear.clear(app);
}

/// 保存原生 renderer 的非空图像与对应场景，便于人工复核而不把单测当成视觉证明。
///
/// @author kongweiguang
fn save_frame(
    window: &Window,
    output: &Path,
    frames: &mut Vec<NativeFrame>,
    theme: &str,
    width: u32,
    surface: &str,
) -> Result<(), io::Error> {
    let image = window.render_to_image().map_err(io::Error::other)?;
    if image.width() == 0 || image.height() == 0 {
        return Err(io::Error::other(
            "native renderer returned a zero-sized image",
        ));
    }
    if !image
        .as_raw()
        .as_chunks::<4>()
        .0
        .iter()
        .any(|pixel| pixel[3] != 0)
    {
        return Err(io::Error::other(
            "native renderer returned a fully transparent image",
        ));
    }
    let file = format!("{theme}-{width}-{surface}.png");
    image.save(output.join(&file)).map_err(io::Error::other)?;
    frames.push(NativeFrame {
        theme: theme.to_owned(),
        width,
        surface: surface.to_owned(),
        file,
    });
    Ok(())
}

/// 保留首次失败供退出后回报，后续收尾错误不覆盖最初定位依据。
///
/// @author kongweiguang
fn record_failure(failure: &Mutex<Option<String>>, message: String) {
    let mut slot = failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_none() {
        *slot = Some(message);
    }
}
