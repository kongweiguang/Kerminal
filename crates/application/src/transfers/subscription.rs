//! 可取消的有界队列唤醒订阅；广播只提示重新查询状态，不承担状态存储。
//!
//! @author kongweiguang

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::transfers::TransferQueueEvent;

/// 保留清理事件与丢帧事实，避免观察者仅凭剩余快照漏掉短暂完成的目录写入。
///
/// @author kongweiguang
pub(crate) async fn receive_transfer_event(
    receiver: &mut broadcast::Receiver<TransferQueueEvent>,
    cancellation: &CancellationToken,
) -> Option<TransferQueueEvent> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => None,
        event = receiver.recv() => match event {
            Ok(event) => Some(event),
            Err(broadcast::error::RecvError::Lagged(_)) => Some(TransferQueueEvent::Resync),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
}
