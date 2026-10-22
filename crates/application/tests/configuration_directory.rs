//! 配置目录应用用例的固定身份、请求回执与取消门禁测试。
//!
//! @author kongweiguang

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kerminal_application::configuration_directory::ConfigurationDirectoryPort;
use kerminal_application::{
    AppError, AppResult, ConfigurationDirectoryKind, ConfigurationDirectoryService,
    OpenConfigurationDirectoryResult,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct FakeDirectoryPort {
    calls: Mutex<Vec<ConfigurationDirectoryKind>>,
}

#[async_trait]
impl ConfigurationDirectoryPort for FakeDirectoryPort {
    /// 记录固定目录身份并在测试取消时拒绝请求，模拟平台端口的协作边界。
    async fn request_open(
        &self,
        kind: ConfigurationDirectoryKind,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("测试目录请求已取消"));
        }
        self.calls.lock().expect("calls mutex").push(kind);
        Ok(())
    }
}

/// 应用回执只表明系统打开请求已接受，不伪装成文件管理器窗口已显示。
#[tokio::test]
async fn returns_request_accepted_for_fixed_snippets_kind() {
    let port = Arc::new(FakeDirectoryPort::default());
    let service = ConfigurationDirectoryService::new(port.clone());

    let result = service
        .open(
            ConfigurationDirectoryKind::Snippets,
            CancellationToken::new(),
        )
        .await
        .expect("open request");

    assert_eq!(
        result,
        OpenConfigurationDirectoryResult::RequestAccepted {
            kind: ConfigurationDirectoryKind::Snippets,
        }
    );
    assert_eq!(
        *port.calls.lock().expect("calls mutex"),
        [ConfigurationDirectoryKind::Snippets]
    );
}

/// 偏好页的恢复入口只请求打开固定配置根，不扩展为任意路径。
///
/// @author kongweiguang
#[tokio::test]
async fn opens_the_fixed_configuration_root_kind() {
    let port = Arc::new(FakeDirectoryPort::default());
    let service = ConfigurationDirectoryService::new(port.clone());

    let result = service
        .open(ConfigurationDirectoryKind::Root, CancellationToken::new())
        .await
        .expect("open request");

    assert_eq!(
        result,
        OpenConfigurationDirectoryResult::RequestAccepted {
            kind: ConfigurationDirectoryKind::Root,
        }
    );
    assert_eq!(
        *port.calls.lock().expect("calls mutex"),
        [ConfigurationDirectoryKind::Root]
    );
}

/// 已取消的调用在应用边界即返回，不会启动平台文件管理器。
#[tokio::test]
async fn cancellation_prevents_open_request() {
    let port = Arc::new(FakeDirectoryPort::default());
    let service = ConfigurationDirectoryService::new(port.clone());
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    assert!(
        service
            .open(ConfigurationDirectoryKind::Snippets, cancellation)
            .await
            .is_err()
    );
    assert!(port.calls.lock().expect("calls mutex").is_empty());
}
