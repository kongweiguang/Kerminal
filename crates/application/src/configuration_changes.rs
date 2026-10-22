//! 配置外部变更通知端口，供桌面界面集中刷新并保留各自的未保存草稿。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_domain::preferences::ConfigurationRecoveryState;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// 一批已合并的配置文件变化；路径相对配置根且不包含文件正文或凭据。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigurationChangeEvent {
    pub relative_paths: Vec<String>,
    pub recovery_state: ConfigurationRecoveryState,
    pub full_rescan_required: bool,
}

/// 通知通道被多个面板共享；接收端落后时必须重读完整配置快照。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigurationChangeMessage {
    Changed(ConfigurationChangeEvent),
    RescanRequired,
    Cancelled,
    Closed,
}

/// 基础设施 watcher 只发布经过根目录约束和恢复校验的配置变化。
pub trait ConfigurationChangeRepository: Send + Sync + 'static {
    /// 为每个 UI 或外部消费者建立独立接收游标，避免订阅者互相分走事件。
    fn subscribe_changes(&self) -> broadcast::Receiver<ConfigurationChangeEvent>;

    /// 在订阅落后或请求全量刷新时提供最新恢复门禁，不要求消费者重新解释文件。
    fn recovery_state(&self) -> ConfigurationRecoveryState;
}

/// 可跨页面共享的应用通知服务；订阅本身不启动 watcher，也不触发文件读取。
#[derive(Clone)]
pub struct ConfigurationChangeService {
    repository: Arc<dyn ConfigurationChangeRepository>,
}

impl ConfigurationChangeService {
    /// 注入 desktop 已启动的共享 watcher 仓储，保持 watcher 生命周期由 composition root 管理。
    pub fn new(repository: Arc<dyn ConfigurationChangeRepository>) -> Self {
        Self { repository }
    }

    /// 返回独立订阅；订阅者应在所属 panel 关闭或应用退出时丢弃接收器。
    pub fn subscribe(&self) -> ConfigurationChangeSubscription {
        ConfigurationChangeSubscription {
            receiver: self.repository.subscribe_changes(),
        }
    }

    /// 返回共享仓储当前恢复态，订阅者处理 `RescanRequired` 时据此更新写入门禁。
    pub fn recovery_state(&self) -> ConfigurationRecoveryState {
        self.repository.recovery_state()
    }
}

/// 一个接收游标；有界通道溢出时显式要求完整重读，避免静默漏掉外部修改。
pub struct ConfigurationChangeSubscription {
    receiver: broadcast::Receiver<ConfigurationChangeEvent>,
}

impl ConfigurationChangeSubscription {
    /// 同时等待事件和取消信号，保证页面销毁后后台订阅 future 能及时退出。
    pub async fn recv(&mut self, cancellation: CancellationToken) -> ConfigurationChangeMessage {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => ConfigurationChangeMessage::Cancelled,
            result = self.receiver.recv() => match result {
                Ok(event) => ConfigurationChangeMessage::Changed(event),
                Err(broadcast::error::RecvError::Lagged(_)) => ConfigurationChangeMessage::RescanRequired,
                Err(broadcast::error::RecvError::Closed) => ConfigurationChangeMessage::Closed,
            },
        }
    }
}
