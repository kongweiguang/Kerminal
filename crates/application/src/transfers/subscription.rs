//! 可取消的有界队列唤醒订阅；广播只提示重新查询状态，不承担状态存储。
//!
//! @author kongweiguang

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::transfers::TransferQueueEvent;

/// 等待一次队列变化并优先响应取消；lag 表示调用方应重新读取完整任务快照。
pub(crate) async fn receive_transfer_wakeup(
    receiver: &mut broadcast::Receiver<TransferQueueEvent>,
    cancellation: &CancellationToken,
) -> Option<()> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => None,
        event = receiver.recv() => match event {
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => Some(()),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
}
