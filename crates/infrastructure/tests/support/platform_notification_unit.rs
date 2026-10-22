//! 系统通知 adapter 的边界测试；故意不向真实桌面发送通知。
//!
//! @author kongweiguang

use crate::platform::notification::NotifyRustAdapter;
use kerminal_application::platform::NotificationPort;
use kerminal_application::{AppError, platform::NotificationPayload};

/// 原生边界拒绝越长文案，避免绕过 application 策略直接调用 OS 通知 API。
#[tokio::test]
async fn adapter_rejects_unbounded_payload_without_touching_os() {
    let result = NotifyRustAdapter
        .send(NotificationPayload {
            title: "T".repeat(73),
            body: "body".to_owned(),
        })
        .await;
    assert!(matches!(result, Err(AppError::Invalid { .. })));
}

/// 空标题与 NUL 字节作为格式错误处理，不在 notification crate 中补默认值。
#[tokio::test]
async fn adapter_rejects_invalid_native_text() {
    let empty_title = NotifyRustAdapter
        .send(NotificationPayload {
            title: "  ".to_owned(),
            body: "body".to_owned(),
        })
        .await;
    let nul_body = NotifyRustAdapter
        .send(NotificationPayload {
            title: "Kerminal".to_owned(),
            body: "unsafe\0body".to_owned(),
        })
        .await;
    assert!(matches!(empty_title, Err(AppError::Invalid { .. })));
    assert!(matches!(nul_body, Err(AppError::Invalid { .. })));
}
