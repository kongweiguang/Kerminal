//! 跨平台原生托盘适配器，Linux 选择 tray-icon 的 GTK-free KSNI 后端。  
//!
//! @author kongweiguang

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use kerminal_application::{
    AppError, AppResult,
    platform::{PlatformAvailability, TrayAction, TrayPort},
};
use tokio::sync::mpsc;
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

const TRAY_ID: &str = "io.github.kongweiguang.kerminal";
const SHOW_ID: &str = "kerminal.tray.show";
const HIDE_ID: &str = "kerminal.tray.hide";
const QUIT_ID: &str = "kerminal.tray.quit";
const MAX_EVENTS_PER_SOURCE_PER_TURN: usize = 32;

/// 只在 GPUI 对应的平台 UI 线程上持有原生对象，Drop 时停止事件桥并释放托盘句柄。
pub struct NativeTrayAdapter {
    icon: Option<TrayIcon>,
    stop_bridge: Option<Arc<AtomicBool>>,
    bridge_thread: Option<JoinHandle<()>>,
    hide_window_supported: bool,
}

impl NativeTrayAdapter {
    /// 创建托盘控制器，并把真实窗口隐藏能力作为菜单可用性的唯一依据。
    pub fn new(hide_window_supported: bool) -> Self {
        Self {
            icon: None,
            stop_bridge: None,
            bridge_thread: None,
            hide_window_supported,
        }
    }
}

impl Default for NativeTrayAdapter {
    /// 默认关闭隐藏菜单，避免未注入窗口能力时显示无法兑现的托盘动作。
    fn default() -> Self {
        Self::new(false)
    }
}

impl TrayPort for NativeTrayAdapter {
    /// 创建可用菜单并桥接全局事件；分批轮询让持续托盘事件不能饿死菜单事件或关闭标记。
    fn install(&mut self, actions: mpsc::Sender<TrayAction>) -> AppResult<()> {
        if self.icon.is_some() {
            return Err(AppError::conflict("系统托盘已安装"));
        }

        let show = MenuItem::with_id(SHOW_ID, "显示 Kerminal", true, None);
        let (hide_label, hide_enabled) = if self.hide_window_supported {
            ("隐藏 Kerminal", true)
        } else {
            ("隐藏窗口不可用", false)
        };
        let hide = MenuItem::with_id(HIDE_ID, hide_label, hide_enabled, None);
        let separator = PredefinedMenuItem::separator();
        let quit = MenuItem::with_id(QUIT_ID, "退出 Kerminal", true, None);
        let menu = Menu::with_items(&[&show, &hide, &separator, &quit])
            .map_err(|error| AppError::infrastructure_with_source("托盘菜单创建失败", error))?;
        let tray = TrayIconBuilder::new()
            .with_id(TRAY_ID)
            .with_tooltip("Kerminal")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_icon(kerminal_tray_icon()?)
            .build()
            .map_err(|error| AppError::infrastructure_with_source("系统托盘安装失败", error))?;

        let tray_id = tray.id().clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let tray_events = TrayIconEvent::receiver().clone();
        let menu_events = MenuEvent::receiver().clone();
        let bridge_thread = thread::Builder::new()
            .name("kerminal-tray-events".to_owned())
            .spawn(move || {
                tracing::debug!(target: "desktop.tray", event = "TRAY_BRIDGE_THREAD_STARTED");
                run_event_bridge(
                    &thread_stop,
                    || tray_events.try_recv().ok(),
                    |event| {
                        if event.id() == &tray_id && tray_event_requests_show(&event) {
                            dispatch_tray_action(&actions, &thread_stop, TrayAction::Show);
                        }
                    },
                    || menu_events.try_recv().ok(),
                    |event| {
                        if let Some(action) = tray_action_for_menu_id(event.id().as_ref()) {
                            dispatch_tray_action(&actions, &thread_stop, action);
                        }
                    },
                );
                tracing::debug!(target: "desktop.tray", event = "TRAY_BRIDGE_THREAD_EXITED");
            })
            .map_err(|error| {
                AppError::infrastructure_with_source("托盘事件桥接线程启动失败", error)
            })?;

        self.icon = Some(tray);
        self.stop_bridge = Some(stop);
        self.bridge_thread = Some(bridge_thread);
        Ok(())
    }

    /// Linux KSNI 可以无需 GTK 主循环工作，但当前桌面仍须提供 SNI watcher。
    fn availability(&self) -> PlatformAvailability {
        #[cfg(target_os = "linux")]
        {
            PlatformAvailability::Conditional {
                reason: "Linux 使用纯 D-Bus StatusNotifierItem；桌面需要 SNI watcher，GNOME 通常需安装 AppIndicator 扩展。".to_owned(),
            }
        }
        #[cfg(target_os = "macos")]
        {
            PlatformAvailability::Conditional {
                reason: "macOS 托盘必须在主线程的 GPUI 事件循环开始运行后安装。".to_owned(),
            }
        }
        #[cfg(target_os = "windows")]
        {
            PlatformAvailability::Conditional {
                reason: "Windows 托盘必须在 GPUI 原生事件循环线程安装。".to_owned(),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            PlatformAvailability::Unsupported {
                reason: "当前目标平台没有配置原生系统托盘实现。".to_owned(),
            }
        }
    }
}

impl Drop for NativeTrayAdapter {
    /// 先请求停止并 join 仍持有 sender 的事件桥，再释放 native handle，保持资源所有权顺序。
    fn drop(&mut self) {
        if let Some(stop) = self.stop_bridge.take() {
            tracing::debug!(target: "desktop.tray", event = "TRAY_BRIDGE_STOP_REQUESTED");
            stop.store(true, Ordering::Release);
        }
        if let Some(thread) = self.bridge_thread.take() {
            tracing::debug!(target: "desktop.tray", event = "TRAY_BRIDGE_JOIN_BEGIN");
            match thread.join() {
                Ok(()) => {
                    tracing::debug!(target: "desktop.tray", event = "TRAY_BRIDGE_JOIN_COMPLETE")
                }
                Err(_) => {
                    tracing::warn!(target: "desktop.tray", event = "TRAY_BRIDGE_JOIN_PANICKED")
                }
            }
        }
        if self.icon.is_some() {
            tracing::debug!(target: "desktop.tray", event = "TRAY_NATIVE_ICON_RELEASE_BEGIN");
            drop(self.icon.take());
            tracing::debug!(target: "desktop.tray", event = "TRAY_NATIVE_ICON_RELEASE_COMPLETE");
        }
    }
}

/// 按来源轮流处理有限批次，避免一个持续产出的全局队列屏蔽另一类事件或停止请求。
fn run_event_bridge<T, M>(
    stop: &AtomicBool,
    mut try_tray_event: impl FnMut() -> Option<T>,
    mut handle_tray_event: impl FnMut(T),
    mut try_menu_event: impl FnMut() -> Option<M>,
    mut handle_menu_event: impl FnMut(M),
) {
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }

        let tray_count = drain_event_batch(stop, &mut try_tray_event, &mut handle_tray_event);
        if stop.load(Ordering::Acquire) {
            break;
        }

        let menu_count = drain_event_batch(stop, &mut try_menu_event, &mut handle_menu_event);
        if stop.load(Ordering::Acquire) {
            break;
        }

        if tray_count == 0 && menu_count == 0 {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// 每批同时受事件数和共享 stop 标记约束，使任意来源持续非空时仍能有界返回外层。
fn drain_event_batch<E>(
    stop: &AtomicBool,
    try_event: &mut impl FnMut() -> Option<E>,
    handle_event: &mut impl FnMut(E),
) -> usize {
    let mut handled = 0;
    while handled < MAX_EVENTS_PER_SOURCE_PER_TURN && !stop.load(Ordering::Acquire) {
        let Some(event) = try_event() else {
            break;
        };
        handled += 1;
        handle_event(event);
    }
    handled
}

/// 将三种固定菜单 ID 映射为应用动作，未知菜单项不向窗口层传播。
fn tray_action_for_menu_id(menu_id: &str) -> Option<TrayAction> {
    match menu_id {
        SHOW_ID => Some(TrayAction::Show),
        HIDE_ID => Some(TrayAction::Hide),
        QUIT_ID => Some(TrayAction::Quit),
        _ => None,
    }
}

/// 单击托盘左键显示主窗口；平台不支持的事件和按钮状态由默认分支忽略。
fn tray_event_requests_show(event: &TrayIconEvent) -> bool {
    matches!(
        event,
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } | TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        }
    )
}

/// 在独立事件桥中对满队列做可中断背压，避免丢失动作，也避免关闭时 join 永久等待。
fn dispatch_tray_action(
    actions: &mpsc::Sender<TrayAction>,
    stop: &AtomicBool,
    mut action: TrayAction,
) {
    loop {
        match actions.try_send(action) {
            Ok(()) => return,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!(target: "desktop.tray", "tray action queue is closed");
                return;
            }
            Err(mpsc::error::TrySendError::Full(returned)) => {
                action = returned;
                if stop.load(Ordering::Acquire) {
                    tracing::debug!(target: "desktop.tray", "tray action abandoned during shutdown");
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// 内嵌原版 PNG 并复用现有 PNG 解码依赖，使托盘与应用图标一致且不依赖安装目录或工作目录。
fn kerminal_tray_icon() -> AppResult<Icon> {
    let image = image::load_from_memory_with_format(
        include_bytes!("../../../../resources/icons/32x32.png"),
        image::ImageFormat::Png,
    )
    .map_err(|error| AppError::infrastructure_with_source("系统托盘图标解码失败", error))?
    .into_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height)
        .map_err(|error| AppError::infrastructure_with_source("系统托盘图标创建失败", error))
}

#[cfg(test)]
#[path = "../../tests/support/platform_tray_unit.rs"]
mod tests;
