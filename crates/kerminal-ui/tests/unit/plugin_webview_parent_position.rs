//! @author kongweiguang

//! 位置通知必须幂等且可恢复；这些测试不启动系统 WebView 或访问用户桌面。

use super::ParentPosition;
use kerminal_application::AppError;

/// 重复 resize/布局事件不能反复调用 COM；只有本容器的真实屏幕原点变化才产生通知。
#[test]
fn parent_position_notifies_only_when_origin_changes() {
    let mut position = ParentPosition {
        origin: (40, 72),
        notifications: 0,
    };
    position
        .synchronize((40, 72), || panic!("unchanged position must not notify"))
        .unwrap();
    position.synchronize((64, 72), || Ok(())).unwrap();
    position
        .synchronize((64, 72), || panic!("repeated bounds must not notify"))
        .unwrap();
    assert_eq!(position.origin, (64, 72));
    assert_eq!(position.notifications, 1);
}

/// 通知失败后保留原位置，恢复时仍须向引擎提交同一移动，不能假报已同步。
#[test]
fn failed_parent_position_notification_remains_retryable() {
    let mut position = ParentPosition {
        origin: (40, 72),
        notifications: 0,
    };
    assert!(
        position
            .synchronize((64, 72), || Err(AppError::infrastructure(
                "fixture failure"
            )))
            .is_err()
    );
    assert_eq!(position.origin, (40, 72));
    assert_eq!(position.notifications, 0);
    position.synchronize((64, 72), || Ok(())).unwrap();
    assert_eq!(position.origin, (64, 72));
    assert_eq!(position.notifications, 1);
}
