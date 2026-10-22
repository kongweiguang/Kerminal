//! 串口目录应用用例的取消与结果边界。
//!
//! @author kongweiguang

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{SerialPortDescriptor, SerialPortRepository, SerialPortService},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

struct FakeSerialPortRepository {
    calls: AtomicUsize,
    fail: bool,
}

struct WaitingSerialPortRepository {
    started: Notify,
}

#[async_trait]
impl SerialPortRepository for WaitingSerialPortRepository {
    /// 等待本次取消令牌，确认取消可以从应用用例传到正在枚举的目录适配器。
    /// @author kongweiguang
    async fn list(&self, cancel: CancellationToken) -> AppResult<Vec<SerialPortDescriptor>> {
        self.started.notify_one();
        cancel.cancelled().await;
        Err(AppError::cancelled("测试串口枚举已取消"))
    }
}

#[async_trait]
impl SerialPortRepository for FakeSerialPortRepository {
    /// 测试目录返回固定快照或受控错误，不访问当前机器的串口设备。
    /// @author kongweiguang
    async fn list(&self, cancel: CancellationToken) -> AppResult<Vec<SerialPortDescriptor>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("测试串口目录请求已取消"));
        }
        if self.fail {
            return Err(AppError::infrastructure("测试串口目录不可用"));
        }
        Ok(vec![SerialPortDescriptor {
            name: "COM17".to_owned(),
            manufacturer: Some("Test vendor".to_owned()),
            product: Some("Test adapter".to_owned()),
            vendor_id: Some(0x1234),
            product_id: Some(0x5678),
        }])
    }
}

/// 确认用例原样传递设备身份和硬件元数据，UI 无需绑定驱动 crate。
/// @author kongweiguang
#[tokio::test]
async fn lists_platform_descriptors_without_rewriting_identity() {
    let repository = Arc::new(FakeSerialPortRepository {
        calls: AtomicUsize::new(0),
        fail: false,
    });
    let service = SerialPortService::new(repository.clone());

    let ports = service
        .list(CancellationToken::new())
        .await
        .expect("serial directory is available");

    assert_eq!(ports.len(), 1);
    assert_eq!(ports[0].name, "COM17");
    assert_eq!(ports[0].manufacturer.as_deref(), Some("Test vendor"));
    assert_eq!(ports[0].product.as_deref(), Some("Test adapter"));
    assert_eq!(ports[0].vendor_id, Some(0x1234));
    assert_eq!(ports[0].product_id, Some(0x5678));
    assert_eq!(repository.calls.load(Ordering::Relaxed), 1);
}

/// 已取消的面板请求必须在进入阻塞设备枚举前结束。
/// @author kongweiguang
#[tokio::test]
async fn does_not_enumerate_after_request_was_cancelled() {
    let repository = Arc::new(FakeSerialPortRepository {
        calls: AtomicUsize::new(0),
        fail: false,
    });
    let service = SerialPortService::new(repository.clone());
    let cancel = CancellationToken::new();
    cancel.cancel();

    let error = service.list(cancel).await.expect_err("cancel is respected");

    assert_eq!(error.public_code().as_str(), "cancelled");
    assert_eq!(repository.calls.load(Ordering::Relaxed), 0);
}

/// 枚举开始后取消也必须到达 repository，不能只在 UI 丢弃返回结果。
/// @author kongweiguang
#[tokio::test]
async fn forwards_cancellation_to_an_in_flight_enumeration() {
    let repository = Arc::new(WaitingSerialPortRepository {
        started: Notify::new(),
    });
    let service = SerialPortService::new(repository.clone());
    let cancel = CancellationToken::new();
    let task = {
        let service = service.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { service.list(cancel).await })
    };

    repository.started.notified().await;
    cancel.cancel();
    let error = task
        .await
        .expect("enumeration task joins")
        .expect_err("cancelled enumeration is not a successful empty list");

    assert_eq!(error.public_code().as_str(), "cancelled");
}

/// 目录错误不能被转换成“没有设备”，否则用户会失去区分空列表和枚举失败的依据。
/// @author kongweiguang
#[tokio::test]
async fn preserves_repository_failure_category() {
    let repository = Arc::new(FakeSerialPortRepository {
        calls: AtomicUsize::new(0),
        fail: true,
    });
    let service = SerialPortService::new(repository.clone());

    let error = service
        .list(CancellationToken::new())
        .await
        .expect_err("repository failure is surfaced");

    assert_eq!(error.public_code().as_str(), "infrastructure");
    assert_eq!(repository.calls.load(Ordering::Relaxed), 1);
}
