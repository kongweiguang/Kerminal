//! GPUI 主线程托盘生命周期与 Root 退出门禁桥接。
//!
//! @author kongweiguang

use gpui_kit::{AnyWindowHandle, App, AsyncApp, Entity, Global};
use kerminal_application::{
    AppResult,
    platform::{PlatformAvailability, TrayAction, TrayPort},
};
use kerminal_infrastructure::platform::tray::NativeTrayAdapter;
use kerminal_ui::{QuitIntent, RootView};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// App Global 持有非 Send 原生托盘对象，确保安装与释放都发生在 GPUI 主线程。
struct TrayLifecycle {
    _adapter: NativeTrayAdapter,
    cancellation: CancellationToken,
}

impl Global for TrayLifecycle {}

/// 将平台能力与实际托盘 owner 分开返回，避免 Conditional 状态被误当作已注册托盘。
pub(super) struct TrayInstallReceipt {
    pub(super) availability: PlatformAvailability,
    pub(super) owner_installed: bool,
}

impl Drop for TrayLifecycle {
    /// 先取消 UI receiver，再释放 tray adapter 让其停止并 join 原生事件桥线程。
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// 在 GPUI 主线程安装托盘；隐藏菜单依赖真实窗口能力，owner 状态只在 Global 注册后报告。
/// @author kongweiguang
pub fn install(
    cx: &mut App,
    root: Entity<RootView>,
    window: AnyWindowHandle,
) -> AppResult<TrayInstallReceipt> {
    let hide_window_supported = window
        .update(cx, |_, window, _| window.supports_visibility_control())
        .unwrap_or(false);
    let mut adapter = NativeTrayAdapter::new(hide_window_supported);
    let availability = adapter.availability();
    if matches!(availability, PlatformAvailability::Unsupported { .. }) {
        return Ok(TrayInstallReceipt {
            availability,
            owner_installed: false,
        });
    }

    let (actions, mut receiver) = mpsc::channel(16);
    adapter.install(actions)?;
    let cancellation = CancellationToken::new();
    cx.set_global(TrayLifecycle {
        _adapter: adapter,
        cancellation: cancellation.clone(),
    });

    cx.spawn(async move |cx| {
        loop {
            let action = tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                action = receiver.recv() => action,
            };
            let Some(action) = action else {
                break;
            };
            dispatch_action(action, &root, window, cx);
        }
    })
    .detach();

    Ok(TrayInstallReceipt {
        availability,
        owner_installed: true,
    })
}

/// 将托盘动作映射到平台可见性能力；不支持隐藏的窗口不会伪装成隐藏或自动最小化。
fn dispatch_action(
    action: TrayAction,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    cx: &mut AsyncApp,
) {
    match action {
        TrayAction::Show => set_window_visibility(true, root, window, cx),
        TrayAction::Hide => set_window_visibility(false, root, window, cx),
        TrayAction::Quit => {
            root.update(cx, |root, cx| {
                root.request_quit(QuitIntent::Exit, cx);
            });
        }
    }
}

/// 只有后端接受真实 hide/show 请求后才更新用户状态；显示时再请求 GPUI 激活窗口。
fn set_window_visibility(
    visible: bool,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    cx: &mut AsyncApp,
) {
    let result = window.update(cx, |_, window, _| {
        let supported = window.supports_visibility_control();
        let applied = supported && window.set_window_visibility(visible).is_ok();
        if supported && applied && visible {
            window.activate_window();
        }
        (supported, applied)
    });

    match result {
        Ok((true, true)) => {}
        Ok((false, _)) => {
            tracing::warn!(event = "TRAY_WINDOW_VISIBILITY_UNSUPPORTED");
            report_visibility_failure(visible, root, cx);
        }
        Ok((true, false)) | Err(_) => {
            tracing::warn!(event = "TRAY_WINDOW_VISIBILITY_FAILED");
            report_visibility_failure(visible, root, cx);
        }
    }
}

/// 将平台窗口失败转成不含原生句柄或系统错误正文的可见提示。
/// @author kongweiguang
fn report_visibility_failure(visible: bool, root: &Entity<RootView>, cx: &mut AsyncApp) {
    let message = if visible {
        "窗口恢复请求失败，请从系统窗口列表手动恢复。"
    } else {
        "此平台未能隐藏窗口；窗口仍保持打开状态。"
    };
    root.update(cx, |root, cx| {
        root.report_host_status(message.to_owned(), cx)
    });
}
