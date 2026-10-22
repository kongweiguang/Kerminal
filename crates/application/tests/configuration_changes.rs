//! 配置订阅的取消、lag 恢复与只读门禁查询契约测试。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_application::configuration_changes::{
    ConfigurationChangeEvent, ConfigurationChangeMessage, ConfigurationChangeRepository,
    ConfigurationChangeService,
};
use kerminal_domain::preferences::{
    ConfigurationIssue, ConfigurationMode, ConfigurationRecoveryState,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct TestChangeRepository {
    sender: broadcast::Sender<ConfigurationChangeEvent>,
    recovery: ConfigurationRecoveryState,
}

impl ConfigurationChangeRepository for TestChangeRepository {
    /// 每个订阅者各自取得广播游标，测试可独立模拟 lag。
    fn subscribe_changes(&self) -> broadcast::Receiver<ConfigurationChangeEvent> {
        self.sender.subscribe()
    }

    /// 返回当前测试恢复态，不执行文件 I/O。
    fn recovery_state(&self) -> ConfigurationRecoveryState {
        self.recovery.clone()
    }
}

/// 创建容量为一的事件通道，确保连续更新会触发可观察的 lag 状态。
fn test_service() -> (
    ConfigurationChangeService,
    broadcast::Sender<ConfigurationChangeEvent>,
) {
    let (sender, _) = broadcast::channel(1);
    let repository = Arc::new(TestChangeRepository {
        sender: sender.clone(),
        recovery: ConfigurationRecoveryState {
            mode: ConfigurationMode::ReadOnlyRecovery,
            issues: vec![ConfigurationIssue {
                relative_path: "settings.toml".to_owned(),
                kind: kerminal_domain::preferences::ConfigurationIssueKind::MalformedToml,
                line: Some(2),
                column: Some(1),
            }],
        },
    });
    (ConfigurationChangeService::new(repository), sender)
}

/// 创建不含文件正文的安全配置变更事件。
fn test_event() -> ConfigurationChangeEvent {
    ConfigurationChangeEvent {
        relative_paths: vec!["settings.toml".to_owned()],
        recovery_state: ConfigurationRecoveryState {
            mode: ConfigurationMode::ReadWrite,
            issues: Vec::new(),
        },
        full_rescan_required: false,
    }
}

#[tokio::test]
/// 页面生命周期 token 取消后，等待中的事件 future 立即返回而不依赖全局 watcher 关闭。
async fn cancellation_stops_a_pending_configuration_receive() {
    let (service, _) = test_service();
    let mut subscription = service.subscribe();
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    assert_eq!(
        subscription.recv(cancellation).await,
        ConfigurationChangeMessage::Cancelled
    );
}

#[tokio::test]
/// 广播队列 lag 显式要求全量重读，恢复态可从共享服务同步取回。
async fn lag_requests_rescan_and_exposes_current_recovery_state() {
    let (service, sender) = test_service();
    let mut subscription = service.subscribe();
    sender.send(test_event()).expect("发布首个配置事件");
    sender.send(test_event()).expect("发布第二个配置事件");

    assert_eq!(
        subscription.recv(CancellationToken::new()).await,
        ConfigurationChangeMessage::RescanRequired
    );
    assert_eq!(
        service.recovery_state().mode,
        ConfigurationMode::ReadOnlyRecovery
    );
}
