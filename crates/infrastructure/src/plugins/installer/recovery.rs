//! 失败变更撤销只回收本次遗留的暂存槽；活动目录与用户数据都保持原样。
//! @author kongweiguang

use super::*;
use crate::plugins::{
    layout::{self, LayoutError},
    store::{PluginStoreSnapshot, layout_error},
};

/// 失败意图通过 CAS 撤销；清理异常单独回报，不能继续锁住恢复或绕过未知提交围栏。
pub(super) fn cancel_failed_change(
    store: &PluginStore,
    expected: &PluginStoreSnapshot,
    expected_revision: u64,
    pending: &PendingPluginChange,
) -> AppResult<ExtensionReceipt> {
    let id = pending.plugin_id();
    if expected.state.revision != expected_revision {
        return Err(AppError::conflict("插件状态已更改，请重新读取失败操作"));
    }
    // 失败记录属于该插件的第一笔待处理操作；不能把后续被阻塞的意图当作失败取消。
    let Some(position) = expected
        .state
        .pending
        .iter()
        .position(|change| change.plugin_id() == id)
    else {
        return Err(AppError::conflict("插件失败操作已更改"));
    };
    if &expected.state.pending[position] != pending || !expected.state.failures.contains_key(id) {
        return Err(AppError::conflict("插件失败操作已更改"));
    }
    // 失败必须由当前 pending 的启动应用失败产生才可撤销；包读取失败留下的
    // 旧记录不能借后来排队的新意图清除，否则会取消用户刚发起的别的操作。
    if !expected.state.pending_failures.contains(id) {
        return Err(AppError::conflict("插件失败与待处理操作不匹配"));
    }
    let mut next = expected.state.clone();
    next.pending.remove(position);
    next.failures.remove(id);
    next.pending_failures.remove(id);
    // 必须先提交状态撤销：若 CAS 结果未知，磁盘上的 pending 可能仍引用暂存槽，
    // 提前删除会让下次启动只能记 Missing，永久失去重试证据。
    let snapshot = store
        .compare_and_swap(expected, next)
        .map_err(store_error)?;
    let cleanup_warning = match clean_failed_staging(store.root(), pending) {
        Ok(()) => None,
        // 状态已经撤销，清理失败只作为可见警告；材料原样保留供读回和人工处理。
        Err(error) => Some(AppError::infrastructure_with_source(
            "失败操作已取消；插件暂存残留未清理或清理结果未知，重新安装该版本前需人工处理",
            error,
        )),
    };
    Ok(ExtensionReceipt {
        revision: snapshot.state.revision,
        restart_required: false,
        cleanup_warning,
    })
}

/// 只删除失败安装/更新独占的 `.staging/<id>/<uuid>`；活动目录、备份和用户数据不在此撤销。
fn clean_failed_staging(
    root: &Path,
    pending: &PendingPluginChange,
) -> Result<(), PluginStoreError> {
    let (id, operation_id) = match pending {
        PendingPluginChange::Install { id, staged, .. }
        | PendingPluginChange::Update { id, staged, .. } => (id, staged),
        // 卸载失败时备份属于未完成事务，恢复门禁会阻止走到这里；不做猜测性删除。
        _ => return Ok(()),
    };
    let staging = layout::staging_directory(root, id, operation_id).map_err(layout_error)?;
    match layout::remove_tree_verified(&staging) {
        Ok(()) => Ok(()),
        Err(LayoutError::Io(error)) => Err(PluginStoreError::Io(error)),
        // 危险残留作为可见清理警告保留，不能借此删除链接或未知文件。
        Err(LayoutError::Unsafe(_)) => Err(PluginStoreError::InvalidState),
        // 删除结果未知时保留状态与材料，让调用方读回后再决定。
        Err(error) => Err(layout_error(error)),
    }
}
