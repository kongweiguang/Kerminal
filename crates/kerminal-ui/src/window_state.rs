//! 原生主窗口的 GPUI 事件观察、离屏恢复和有界持久化桥接。
//!
//! @author kongweiguang

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use gpui_kit::{App, Context, Global, Subscription, Window};
use gpui_pre::{Bounds, Pixels, WindowBounds, point, px, size};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor,
    platform::window_state::{
        WindowRect, WindowStateLoad, WindowStateRepository, WindowStateSnapshot,
    },
    platform::{DesktopNotificationService, WindowVisibility as NotificationVisibility},
};
use tokio_util::sync::CancellationToken;

use crate::RootView;

const RESIZE_DEBOUNCE: Duration = Duration::from_millis(350);
const TITLEBAR_HEIGHT: i64 = 44;
const MIN_VISIBLE_TITLEBAR_WIDTH: i64 = 96;
const MIN_VISIBLE_TITLEBAR_HEIGHT: i64 = 20;
const DEFAULT_WINDOW_RECT: WindowRect = WindowRect {
    x: 0,
    y: 0,
    width: 1280,
    height: 820,
};

/// 将 GPUI 订阅保存在 app 全局中，防止窗口生命周期内观察器被提前释放。
pub struct WindowStateBinding {
    handle: WindowStateHandle,
    _subscriptions: Vec<Subscription>,
}

impl Global for WindowStateBinding {}

/// 可克隆的窗口状态写入句柄，由 desktop 在同一 shutdown deadline 下显式 flush。
#[derive(Clone)]
pub struct WindowStateHandle {
    controller: Arc<WindowStateController>,
}

struct WindowStateController {
    repository: Arc<dyn WindowStateRepository>,
    executor: ApplicationTaskExecutor,
    state: Mutex<WindowStateWriteState>,
}

struct WindowStateWriteState {
    latest: WindowStateSnapshot,
    revision: Option<u64>,
    generation: u64,
    persisted_generation: u64,
    worker_running: bool,
    read_only: bool,
    failure: Option<String>,
    completed: CancellationToken,
}

#[derive(Clone, Copy)]
struct VisibilityState {
    visible: bool,
    active: bool,
}

impl WindowStateBinding {
    /// 在原生窗口创建后订阅 bounds、可见性和焦点事件，避免以固定间隔轮询平台状态。
    pub fn install(
        root: &mut RootView,
        window: &mut Window,
        executor: ApplicationTaskExecutor,
        repository: Arc<dyn WindowStateRepository>,
        loaded: WindowStateLoad,
        notifications: Option<Arc<DesktopNotificationService>>,
        cx: &mut Context<RootView>,
    ) -> Self {
        let (snapshot, revision, read_only, failure) = match loaded {
            WindowStateLoad::Restored {
                snapshot, revision, ..
            } => (snapshot, revision, false, None),
            WindowStateLoad::Unavailable { reason } => (
                snapshot_from_window(window, default_snapshot()),
                None,
                true,
                Some(reason.safe_message()),
            ),
            WindowStateLoad::Missing => (
                snapshot_from_window(window, default_snapshot()),
                None,
                false,
                None,
            ),
        };
        let handle = WindowStateHandle {
            controller: Arc::new(WindowStateController {
                repository,
                executor,
                state: Mutex::new(WindowStateWriteState {
                    latest: snapshot,
                    revision,
                    generation: 0,
                    persisted_generation: 0,
                    worker_running: false,
                    read_only,
                    failure: failure.clone(),
                    completed: CancellationToken::new(),
                }),
            }),
        };

        if let Some(message) = failure {
            root.report_host_status(message, cx);
        }

        let bounds_handle = handle.clone();
        let bounds_subscription = cx.observe_window_bounds(window, move |_, window, cx| {
            let previous = bounds_handle.snapshot();
            bounds_handle.capture(snapshot_from_window(window, previous), cx);
        });

        let visibility_state = Arc::new(Mutex::new(VisibilityState {
            visible: window.is_visible(),
            active: window.is_window_active(),
        }));
        apply_notification_visibility(&notifications, Arc::clone(&visibility_state));
        let visibility_notifications = notifications.clone();
        let visibility_state_for_event = Arc::clone(&visibility_state);
        let visibility_subscription =
            cx.observe_window_visibility(window, move |_, visibility, window, _cx| {
                if let Ok(mut current) = visibility_state_for_event.lock() {
                    current.visible = visibility.is_visible();
                    current.active = window.is_window_active();
                }
                apply_notification_visibility(
                    &visibility_notifications,
                    Arc::clone(&visibility_state_for_event),
                );
            });

        let activation_notifications = notifications;
        let activation_state = Arc::clone(&visibility_state);
        let activation_subscription =
            cx.observe_window_activation(window, move |_, window, _cx| {
                if let Ok(mut current) = activation_state.lock() {
                    current.visible = window.is_visible();
                    current.active = window.is_window_active();
                }
                apply_notification_visibility(
                    &activation_notifications,
                    Arc::clone(&activation_state),
                );
            });

        Self {
            handle,
            _subscriptions: vec![
                bounds_subscription,
                visibility_subscription,
                activation_subscription,
            ],
        }
    }

    /// 给 desktop 获取独立的 flush 句柄；GPUI observer 的订阅仍由 Global 保活。
    pub fn handle(&self) -> WindowStateHandle {
        self.handle.clone()
    }
}

impl WindowStateHandle {
    /// bounds 事件出现平台异常值时保留最后有效还原矩形，不以默认值覆盖用户布局。
    fn snapshot(&self) -> WindowStateSnapshot {
        self.controller
            .state
            .lock()
            .map(|state| state.latest)
            .unwrap_or_else(|_| default_snapshot())
    }

    /// 将 bounds 变更合并为单个有界 debounce；后续 CAS 由共享 ApplicationTaskExecutor 执行。
    fn capture(&self, snapshot: WindowStateSnapshot, cx: &mut Context<RootView>) {
        if !snapshot.normal_bounds.is_valid() {
            return;
        }
        let should_start = match self.controller.state.lock() {
            Ok(mut state) => {
                if state.read_only || state.latest == snapshot {
                    return;
                }
                state.latest = snapshot;
                state.generation = match state.generation.checked_add(1) {
                    Some(generation) => generation,
                    None => {
                        state.failure = Some("窗口状态变更过多，已停止自动保存。".to_owned());
                        return;
                    }
                };
                if state.worker_running || state.failure.is_some() {
                    false
                } else {
                    state.worker_running = true;
                    state.completed = CancellationToken::new();
                    true
                }
            }
            Err(_) => {
                tracing::warn!("window state observer lock is unavailable");
                return;
            }
        };
        if should_start {
            let handle = self.clone();
            cx.spawn(async move |view, cx| {
                cx.background_executor().timer(RESIZE_DEBOUNCE).await;
                if let Err(error) = handle.persist_pending().await {
                    let message = error.public_message().to_owned();
                    let _ = view.update(cx, |root, cx| root.report_host_status(message, cx));
                }
            })
            .detach();
        }
    }

    /// 把最后一个待写快照提交完成；desktop 用同一个绝对 deadline 限制此 future 与宿主关闭。
    pub async fn flush(&self) -> AppResult<()> {
        loop {
            let (wait_for_worker, should_run) = {
                let mut state = self
                    .controller
                    .state
                    .lock()
                    .map_err(|_| AppError::infrastructure("窗口状态写入状态不可用"))?;
                if !state.worker_running && state.persisted_generation == state.generation {
                    return Ok(());
                }
                if state.read_only {
                    return Err(AppError::conflict("窗口状态处于只读恢复模式，原文件已保留"));
                }
                if state.worker_running {
                    (state.completed.clone(), false)
                } else {
                    // 退出路径允许重试一次先前失败的 CAS；持久化层仍以原 revision 拒绝未知覆盖。
                    state.failure = None;
                    state.worker_running = true;
                    state.completed = CancellationToken::new();
                    (state.completed.clone(), true)
                }
            };
            if should_run {
                return self.persist_pending().await;
            }
            wait_for_worker.cancelled().await;
        }
    }

    /// 仅经共享 runtime 完成 CAS；revision 冲突、损坏和未知提交均停止重试并保留原件。
    async fn persist_pending(&self) -> AppResult<()> {
        loop {
            let (snapshot, expected_revision, generation) = {
                let mut state = self
                    .controller
                    .state
                    .lock()
                    .map_err(|_| AppError::infrastructure("窗口状态写入状态不可用"))?;
                if state.read_only {
                    state.worker_running = false;
                    state.completed.cancel();
                    return Ok(());
                }
                if state.failure.is_some() {
                    return Err(AppError::infrastructure(
                        "窗口状态写入已停止，请查看窗口状态提示",
                    ));
                }
                if state.persisted_generation == state.generation {
                    state.worker_running = false;
                    state.completed.cancel();
                    return Ok(());
                }
                (state.latest, state.revision, state.generation)
            };

            let repository = Arc::clone(&self.controller.repository);
            let compare_result = self
                .controller
                .executor
                .run(move |_| async move {
                    repository
                        .compare_and_swap(expected_revision, snapshot)
                        .await
                })
                .await;
            match compare_result {
                Ok(revision) => {
                    let more_changes = match self.controller.state.lock() {
                        Ok(mut state) => {
                            state.revision = Some(revision);
                            state.persisted_generation = generation;
                            if state.generation == generation {
                                state.worker_running = false;
                                state.completed.cancel();
                                false
                            } else {
                                true
                            }
                        }
                        Err(_) => {
                            self.finish_worker(Some("窗口状态写入状态不可用".to_owned()));
                            return Err(AppError::infrastructure("窗口状态写入状态不可用"));
                        }
                    };
                    if more_changes {
                        continue;
                    }
                    return Ok(());
                }
                Err(error) => {
                    let message = error.public_message().to_owned();
                    self.finish_worker(Some(message.clone()));
                    return Err(error);
                }
            }
        }
    }

    /// 结束唯一 writer 并唤醒显式 flush 等待者，错误时保留安全摘要供后续关窗门禁展示。
    fn finish_worker(&self, failure: Option<String>) {
        if let Ok(mut state) = self.controller.state.lock() {
            if let Some(message) = failure {
                state.failure = Some(message);
            }
            state.worker_running = false;
            state.completed.cancel();
        }
    }
}

/// 读取恢复快照并把离屏窗口移到可用显示器；没有有效记录时沿用 GPUI 默认布局。
pub fn restored_window_bounds(load: &WindowStateLoad, cx: &App) -> Option<WindowBounds> {
    let WindowStateLoad::Restored { snapshot, .. } = load else {
        return None;
    };
    let display_bounds = display_visible_rects(cx);
    let normal_bounds = restore_rect_for_displays(snapshot.normal_bounds, &display_bounds);
    let bounds = gpui_bounds(normal_bounds);
    Some(if snapshot.fullscreen {
        WindowBounds::Fullscreen(bounds)
    } else if snapshot.maximized {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    })
}

/// 至少保留可抓取的标题栏区域；否则按 primary display 重新居中，避免拔屏后窗口失联。
pub fn restore_rect_for_displays(rect: WindowRect, displays: &[WindowRect]) -> WindowRect {
    let Some(primary) = displays
        .first()
        .copied()
        .filter(|display| display_has_area(*display))
    else {
        return rect;
    };
    let max_width = displays
        .iter()
        .filter(|display| display_has_area(**display))
        .map(|display| display.width)
        .max()
        .unwrap_or(primary.width);
    let max_height = displays
        .iter()
        .filter(|display| display_has_area(**display))
        .map(|display| display.height)
        .max()
        .unwrap_or(primary.height);
    let rect = WindowRect {
        width: rect.width.min(max_width).max(1),
        height: rect.height.min(max_height).max(1),
        ..rect
    };
    if displays
        .iter()
        .any(|display| display_has_area(*display) && titlebar_intersects(rect, *display))
    {
        return rect;
    }
    let rect = clamp_rect_to_display(rect, primary);
    WindowRect {
        x: primary.x.saturating_add(
            i32::try_from(primary.width.saturating_sub(rect.width) / 2).unwrap_or(i32::MAX),
        ),
        y: primary.y.saturating_add(
            i32::try_from(primary.height.saturating_sub(rect.height) / 2).unwrap_or(i32::MAX),
        ),
        ..rect
    }
}

/// 显示器可见区域只需非零面积，避免低分辨率或工作区裁切时失去恢复目标。
fn display_has_area(display: WindowRect) -> bool {
    display.width > 0 && display.height > 0
}

/// 先限制窗口尺寸到主显示器可视区域，再检查标题栏条带是否有可操作的屏幕交集。
fn clamp_rect_to_display(rect: WindowRect, display: WindowRect) -> WindowRect {
    let width = rect.width.min(display.width).max(1);
    let height = rect.height.min(display.height).max(1);
    WindowRect {
        x: rect.x,
        y: rect.y,
        width,
        height,
    }
}

/// 只把窗口顶部标题栏条带与可视显示区域的交集作为可恢复判断，避免主体露出但标题栏离屏。
fn titlebar_intersects(rect: WindowRect, display: WindowRect) -> bool {
    let left = i64::from(rect.x).max(i64::from(display.x));
    let right = (i64::from(rect.x) + i64::from(rect.width))
        .min(i64::from(display.x) + i64::from(display.width));
    let top = i64::from(rect.y).max(i64::from(display.y));
    let bottom =
        (i64::from(rect.y) + TITLEBAR_HEIGHT).min(i64::from(display.y) + i64::from(display.height));
    right - left >= MIN_VISIBLE_TITLEBAR_WIDTH && bottom - top >= MIN_VISIBLE_TITLEBAR_HEIGHT
}

/// 将 GPUI 现有 visible_bounds 转为纯几何 DTO，平台查询只发生在创建窗口的 UI callback。
fn display_visible_rects(cx: &App) -> Vec<WindowRect> {
    let displays = cx.displays();
    let primary_id = cx.primary_display().map(|display| display.id());
    let mut rects = displays
        .iter()
        .filter_map(|display| rect_from_bounds(display.visible_bounds()))
        .collect::<Vec<_>>();
    if let Some(primary_id) = primary_id
        && let Some(index) = displays
            .iter()
            .position(|display| display.id() == primary_id)
        && let Some(primary) = rect_from_bounds(displays[index].visible_bounds())
        && let Some(primary_index) = rects.iter().position(|rect| *rect == primary)
    {
        rects.swap(0, primary_index);
    }
    rects
}

/// 将显示器矩形转换为 GPUI WindowBounds，保持窗口位置与尺寸采用同一像素坐标系。
fn gpui_bounds(rect: WindowRect) -> Bounds<Pixels> {
    Bounds {
        origin: point(px(rect.x as f32), px(rect.y as f32)),
        size: size(px(rect.width as f32), px(rect.height as f32)),
    }
}

/// 读取当前 GPUI 几何与最大化/全屏状态；后两种 bounds 是 GPUI 的还原矩形，转换失败时只保留上一份正常矩形。
fn snapshot_from_window(window: &Window, previous: WindowStateSnapshot) -> WindowStateSnapshot {
    let bounds = window.window_bounds();
    let (normal_bounds, maximized, fullscreen) = match bounds {
        WindowBounds::Windowed(bounds) => (
            rect_from_bounds(bounds).unwrap_or(previous.normal_bounds),
            false,
            false,
        ),
        WindowBounds::Maximized(bounds) => (
            rect_from_bounds(bounds).unwrap_or(previous.normal_bounds),
            true,
            false,
        ),
        WindowBounds::Fullscreen(bounds) => (
            rect_from_bounds(bounds).unwrap_or(previous.normal_bounds),
            false,
            true,
        ),
    };
    WindowStateSnapshot {
        normal_bounds,
        maximized,
        fullscreen,
    }
}

/// 首次启动或当前记录不可用时才采用产品初始布局；后续 bounds 异常会使用已保存矩形。
fn default_snapshot() -> WindowStateSnapshot {
    WindowStateSnapshot {
        normal_bounds: DEFAULT_WINDOW_RECT,
        maximized: false,
        fullscreen: false,
    }
}

/// 仅把有限坐标转换为整数像素，异常平台结果保留调用方提供的上一有效矩形。
fn rect_from_bounds(bounds: Bounds<Pixels>) -> Option<WindowRect> {
    let x = f32::from(bounds.origin.x);
    let y = f32::from(bounds.origin.y);
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    if !x.is_finite() || !y.is_finite() || !width.is_finite() || !height.is_finite() {
        return None;
    }
    let rect = WindowRect {
        x: x.round() as i32,
        y: y.round() as i32,
        width: width.round().clamp(1.0, u32::MAX as f32) as u32,
        height: height.round().clamp(1.0, u32::MAX as f32) as u32,
    };
    rect.is_valid().then_some(rect)
}

/// 把 GPUI 窗口真实可见与激活状态映射到通知策略，不从托盘动作推断窗口仍在前台。
fn apply_notification_visibility(
    notifications: &Option<Arc<DesktopNotificationService>>,
    state: Arc<Mutex<VisibilityState>>,
) {
    let Some(notifications) = notifications else {
        return;
    };
    let Ok(state) = state.lock() else {
        tracing::warn!("window visibility state is unavailable");
        return;
    };
    let visibility = if !state.visible {
        NotificationVisibility::Hidden
    } else if state.active {
        NotificationVisibility::Foreground
    } else {
        NotificationVisibility::Minimized
    };
    if let Err(error) = notifications.set_window_visibility(visibility) {
        tracing::warn!(message = %error.public_message(), "notification visibility update failed");
    }
}
