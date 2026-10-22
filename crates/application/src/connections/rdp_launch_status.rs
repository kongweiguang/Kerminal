//! RDP 外部客户端启动状态的应用层只读端口与类型化观察器。
//!
//! @author kongweiguang

use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{AppError, AppResult, PublicErrorCode};

/// 一次 RDP 启动尝试的短期不透明身份，不暴露主机、用户名或临时文件路径。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RdpLaunchId(Uuid);

impl RdpLaunchId {
    /// 每次新建客户端请求都生成独立 ID，避免按端点复用状态或泄露配置字段。
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// 仅供 adapter 将不透明身份放入内部运行态 registry 的 key。
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for RdpLaunchId {
    /// 默认值仍需是新建的 v4 启动身份，不能落为 `Uuid::default()` 的空标识。
    /// @author kongweiguang
    fn default() -> Self {
        Self::new()
    }
}

/// RDP 本地启动与资源生命周期状态；系统委派不推断远端登录或客户端进程状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RdpLaunchStatus {
    /// 启动请求已经登记；系统 broker 仍可能在异步返回时失败。
    Requested,
    /// 系统 broker 成功完成请求且临时文件已清理，不表示 RDP 客户端已启动或登录。
    DelegatedToSystem,
    /// 本地启动或受管退出失败；只暴露稳定错误码，不包含命令、路径或凭据。
    Failed { code: PublicErrorCode },
    /// 应用实际管理的客户端进程已退出且相关临时文件清理完成。
    Closed,
}

impl RdpLaunchStatus {
    /// 终态不再接受更新；系统委派成功表示 Kerminal 的请求已结束，不表示远端会话终止。
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::DelegatedToSystem | Self::Failed { .. } | Self::Closed
        )
    }
}

/// 一次状态变更的不可变快照；revision 在单次 launch 内单调递增。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RdpLaunchStatusMessage {
    pub launch_id: RdpLaunchId,
    pub revision: u64,
    pub status: RdpLaunchStatus,
}

/// UI 和其它应用调用方通过此端口查询或订阅启动状态，不直接依赖 infra registry。
pub trait RdpLaunchStatusService: Send + Sync + 'static {
    /// 查询仍在保留窗口内的最新状态；已淘汰的终态按未找到返回。
    fn query(&self, launch_id: RdpLaunchId) -> AppResult<RdpLaunchStatusMessage>;

    /// 订阅指定启动 ID 的最新值；无效或已淘汰的身份不得退回到其它启动记录。
    fn subscribe(&self, launch_id: RdpLaunchId) -> AppResult<RdpLaunchStatusSubscription>;
}

struct SubscriptionState {
    receiver: watch::Receiver<RdpLaunchStatusMessage>,
    initial_pending: bool,
}

/// 单消费者最新值订阅；取消只停止观察，不会取消或关闭被观察的客户端。
pub struct RdpLaunchStatusSubscription {
    state: Mutex<SubscriptionState>,
}

impl RdpLaunchStatusSubscription {
    /// 由 application port adapter 包装 watch 接收端，UI 不需要直接持有 Tokio watch 类型。
    pub fn from_receiver(receiver: watch::Receiver<RdpLaunchStatusMessage>) -> Self {
        Self {
            state: Mutex::new(SubscriptionState {
                receiver,
                initial_pending: true,
            }),
        }
    }

    /// 首次立即返回当前快照，后续等待更新；取消仅结束观察并返回脱敏取消错误。
    pub async fn recv(&self, cancellation: CancellationToken) -> AppResult<RdpLaunchStatusMessage> {
        let mut state = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(AppError::cancelled("RDP 启动状态订阅已取消"));
            }
            state = self.state.lock() => state,
        };
        let current = *state.receiver.borrow_and_update();
        if state.initial_pending || current.status.is_terminal() {
            state.initial_pending = false;
            return Ok(current);
        }

        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                Err(AppError::cancelled("RDP 启动状态订阅已取消"))
            }
            changed = state.receiver.changed() => {
                changed
                    .map(|()| *state.receiver.borrow_and_update())
                    .map_err(|_| AppError::not_found("RDP 启动状态已过期"))
            }
        }
    }
}
