//! 通知可见性由同一 service 克隆共享，未知初态必须压制后台通知。
//!
//! @author kongweiguang

use std::sync::Arc;

use async_trait::async_trait;
use kerminal_application::{
    AppResult,
    platform::{
        DesktopNotificationService, NotificationPayload, NotificationPort, WindowVisibility,
    },
    preferences::settings::DesktopNotificationSettings,
};

#[derive(Clone, Copy)]
struct NoopNotificationPort;

#[async_trait]
impl NotificationPort for NoopNotificationPort {
    /// 本测试只检查共享窗口状态，不调用操作系统通知 API。
    async fn send(&self, _payload: NotificationPayload) -> AppResult<()> {
        Ok(())
    }
}

/// 窗口状态 setter 必须作用于 AppServices 可克隆的同一个通知服务实例。
#[test]
fn notification_visibility_is_shared_and_defaults_to_foreground() {
    let service = DesktopNotificationService::new(
        Arc::new(NoopNotificationPort),
        DesktopNotificationSettings::default(),
    );
    let clone = service.clone();

    assert_eq!(
        service.window_visibility().expect("default visibility"),
        WindowVisibility::Foreground
    );
    clone
        .set_window_visibility(WindowVisibility::Minimized)
        .expect("update shared visibility");
    assert_eq!(
        service.window_visibility().expect("shared visibility"),
        WindowVisibility::Minimized
    );
}
