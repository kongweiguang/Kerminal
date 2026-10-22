//! 桌面 composition 的串口 service 装配契约测试。
//!
//! @author kongweiguang

use super::create_serial_port_service;
use tokio_util::sync::CancellationToken;

/// 确认桌面装配的真实平台 service 可在设备访问前响应取消，避免关闭中的表单触发枚举。
#[tokio::test]
async fn composed_serial_port_service_rejects_cancelled_enumeration() {
    let service = create_serial_port_service();
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let error = service
        .list(cancellation)
        .await
        .expect_err("cancelled enumeration must not query platform devices");
    assert_eq!(error.public_code().as_str(), "cancelled");
}
