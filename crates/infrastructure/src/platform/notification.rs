//! notify-rust 系统通知适配器。
//!
//! @author kongweiguang

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    platform::{NotificationPayload, NotificationPort},
};
use notify_rust::Notification;

/// 将同步系统通知调用移出 GPUI/Tokio UI 执行路径。
#[derive(Clone, Copy, Debug, Default)]
pub struct NotifyRustAdapter;

#[async_trait]
impl NotificationPort for NotifyRustAdapter {
    /// 严格限制最后一道原生边界的可见文本长度，并在独立阻塞线程调用 OS 通知 API。
    async fn send(&self, payload: NotificationPayload) -> AppResult<()> {
        if payload.title.trim().is_empty()
            || payload.title.len() > 72
            || payload.body.len() > 180
            || payload.title.contains('\0')
            || payload.body.contains('\0')
        {
            return Err(AppError::invalid("桌面通知文案超出安全限制"));
        }

        tokio::task::spawn_blocking(move || {
            Notification::new()
                .appname("Kerminal")
                .summary(&payload.title)
                .body(&payload.body)
                .show()
                .map(|_| ())
                .map_err(|error| AppError::infrastructure_with_source("系统通知服务不可用", error))
        })
        .await
        .map_err(|error| AppError::infrastructure_with_source("系统通知工作线程失败", error))?
    }
}

#[cfg(test)]
#[path = "../../tests/support/platform_notification_unit.rs"]
mod tests;
