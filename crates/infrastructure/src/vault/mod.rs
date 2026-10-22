//! Encrypted credential vault 与 key 生命周期 adapter。
//!
//! @author kongweiguang

mod error;
mod format;
mod key_management;
mod store;
mod transaction;

pub use error::VaultError;

use crate::storage::StorageCoordinator;

/// 持有共享 storage coordinator 的 vault adapter，可同时实现凭据和 key repository ports。
#[derive(Clone)]
pub struct VaultStore {
    pub(super) storage: StorageCoordinator,
}

impl VaultStore {
    /// 只绑定共享 coordinator；坏 pair 必须以 typed 状态进入显式恢复，不能挡住桌面启动。
    pub fn open(storage: StorageCoordinator) -> Self {
        Self { storage }
    }

    /// 返回配置根目录供组合根验证组件共享同一用户数据范围。
    pub fn root(&self) -> &std::path::Path {
        self.storage.root()
    }

    /// 为 Git sync 持有同一份存储锁，并先恢复未完成的 key/vault pair journal。
    pub fn lock_for_sync(&self) -> Result<crate::storage::StorageLock<'_>, VaultError> {
        transaction::lock_for_sync(&self.storage)
    }
}
