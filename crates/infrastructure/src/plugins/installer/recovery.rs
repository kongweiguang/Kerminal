//! 失败变更撤销保留活动版本，只回收本次失败安装独占的包目录。
//! @author kongweiguang

use super::*;
use crate::plugins::store::PluginStoreSnapshot;

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
    let mut cleanup_warning = None;
    let snapshot = store
        .publish_and_compare(expected, next.clone(), || {
            match clean_failed_package(store.root(), &next, pending) {
                Ok(()) => Ok(()),
                Err(error @ (PluginStoreError::InvalidState | PluginStoreError::Io(_))) => {
                    cleanup_warning = Some(AppError::infrastructure_with_source(
                        "失败操作已取消；插件包残留未清理，重新安装该版本前需人工处理",
                        error,
                    ));
                    Ok(())
                }
                Err(error) => Err(error),
            }
        })
        .map_err(store_error)?;
    Ok(ExtensionReceipt {
        revision: snapshot.state.revision,
        restart_required: false,
        cleanup_warning,
    })
}

/// 只清理失败安装/更新新发布且已无引用的版本；回退目标与活动/上一版本必须保留。
fn clean_failed_package(
    root: &Path,
    state: &PluginStoreState,
    pending: &PendingPluginChange,
) -> Result<(), PluginStoreError> {
    let (id, version) = match pending {
        PendingPluginChange::Install { id, to, .. }
        | PendingPluginChange::Update { id, to, .. } => (id, to),
        _ => return Ok(()),
    };
    if state.plugins.get(id).is_some_and(|installed| {
        &installed.active == version || installed.previous.as_ref() == Some(version)
    }) || state.pending.iter().any(|change| {
        change.plugin_id() == id
            && match change {
                PendingPluginChange::Install { to, .. }
                | PendingPluginChange::Update { to, .. }
                | PendingPluginChange::Rollback { to, .. } => to == version,
                _ => false,
            }
    }) {
        return Ok(());
    }
    let mut directory = root.to_path_buf();
    // 不使用 staged 字符串，也不创建缺失路径；每级都必须位于宿主推导的真实目录内。
    let version = version.to_string();
    for segment in ["", "plugins", "packages", id.as_str(), &version] {
        directory.push(segment);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() && safe_metadata(&metadata) => {}
            Ok(_) => return Err(PluginStoreError::InvalidState),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(PluginStoreError::Io(error)),
        }
    }
    validate_cleanup_tree(&directory)?;
    fs::remove_dir_all(directory).map_err(PluginStoreError::Io)
}

/// 删除前有界清点整棵目录，拒绝链接和重解析点，损坏文件内容本身不妨碍重新安装。
fn validate_cleanup_tree(root: &Path) -> Result<(), PluginStoreError> {
    let mut directories = vec![root.to_path_buf()];
    let mut count = 0usize;
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory).map_err(PluginStoreError::Io)? {
            let entry = entry.map_err(PluginStoreError::Io)?;
            count += 1;
            let metadata = fs::symlink_metadata(entry.path()).map_err(PluginStoreError::Io)?;
            if count > kerminal_plugin_protocol::MAX_PACKAGE_ENTRIES || !safe_metadata(&metadata) {
                return Err(PluginStoreError::InvalidState);
            }
            if metadata.is_dir() {
                directories.push(entry.path());
            } else if !metadata.is_file() {
                return Err(PluginStoreError::InvalidState);
            }
        }
    }
    Ok(())
}
