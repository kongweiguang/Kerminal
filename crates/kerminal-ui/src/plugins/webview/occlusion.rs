//! 自绘覆盖层协调同窗口内 WebView；不将其他窗口的遮挡传播给当前实例。
//!
//! @author kongweiguang
use gpui_kit::{App, Entity, Global, WeakEntity, Window, WindowId};
use kerminal_application::AppResult;

use super::PluginWebView;

#[derive(Default)]
struct Registry {
    windows: Vec<WindowEntries>,
}
struct WindowEntries {
    id: WindowId,
    blocked: bool,
    foreground: Option<String>,
    views: Vec<WeakEntity<PluginWebView>>,
}
impl Global for Registry {}

/// 注册新页面时同步当前窗口的遮挡状态，弹层打开期间创建的页面不能穿透弹层。
pub(super) fn register(
    view: &Entity<PluginWebView>,
    window: &mut Window,
    cx: &mut App,
) -> AppResult<()> {
    if cx.try_global::<Registry>().is_none() {
        cx.set_global(Registry::default());
    }
    let id = window.window_handle().window_id();
    let instance_id = view.read(cx).instance_id.clone();
    let blocked = {
        let registry = cx.global_mut::<Registry>();
        if let Some(entry) = registry.windows.iter_mut().find(|entry| entry.id == id) {
            entry.views.retain(|view| view.upgrade().is_some());
            entry.views.push(view.downgrade());
            entry.blocked && entry.foreground.as_deref() != Some(instance_id.as_str())
        } else {
            registry.windows.push(WindowEntries {
                id,
                blocked: false,
                foreground: None,
                views: vec![view.downgrade()],
            });
            false
        }
    };
    if blocked {
        view.update(cx, |view, cx| view.set_occluded(true, window, cx))?;
    }
    Ok(())
}

/// Root 每帧传入合并状态，只在状态改变时通知网页；释放前保留快照和原子页面实例。
pub(super) fn set_window_occluded(
    window: &mut Window,
    cx: &mut App,
    blocked: bool,
) -> AppResult<()> {
    set_window_occluded_except(window, cx, blocked, None)
}

/// 紧凑工具抽屉内的网页仍是前景表面；只遮挡它后面的网页，其余模态不授予例外。
pub(super) fn set_window_occluded_except(
    window: &mut Window,
    cx: &mut App,
    blocked: bool,
    foreground: Option<String>,
) -> AppResult<()> {
    if cx.try_global::<Registry>().is_none() {
        cx.set_global(Registry::default());
    }
    let id = window.window_handle().window_id();
    let views = {
        let registry = cx.global_mut::<Registry>();
        if let Some(entry) = registry.windows.iter_mut().find(|entry| entry.id == id) {
            if entry.blocked == blocked && entry.foreground == foreground {
                return Ok(());
            }
            entry.blocked = blocked;
            entry.foreground = foreground.clone();
            entry.views.retain(|view| view.upgrade().is_some());
            entry
                .views
                .iter()
                .filter_map(WeakEntity::upgrade)
                .collect::<Vec<_>>()
        } else {
            registry.windows.push(WindowEntries {
                id,
                blocked,
                foreground: foreground.clone(),
                views: Vec::new(),
            });
            Vec::new()
        }
    };
    let mut first_error = None;
    for view in views {
        if let Err(error) = view.update(cx, |view, cx| {
            view.set_occluded(
                blocked && foreground.as_deref() != Some(&view.instance_id),
                window,
                cx,
            )
        }) && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}
