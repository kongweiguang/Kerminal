//! 插件私有 JSON 状态使用共享存储锁和同目录原子提交。
//! @author kongweiguang

use super::{ConfigurationStore, ensure_active};
use crate::storage::CommitMode;
use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    plugin_settings::{MAX_PLUGIN_STATE_BYTES, PluginStateRepository},
};
use kerminal_domain::plugins::PluginId;
use serde_json::Value;
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

/// 每一级都拒绝符号链接和非目录，避免将插件身份映射到其他插件或用户路径。
fn data_directory(root: &Path, plugin: &PluginId, create: bool) -> AppResult<Option<PathBuf>> {
    let mut directory = root.to_path_buf();
    for component in ["plugins", "data", plugin.as_str()] {
        directory.push(component);
        let metadata = match fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
                fs::create_dir(&directory).map_err(|error| {
                    AppError::infrastructure_with_source("创建插件数据目录失败", error)
                })?;
                fs::symlink_metadata(&directory).map_err(|error| {
                    AppError::infrastructure_with_source("检查插件数据目录失败", error)
                })?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(AppError::infrastructure_with_source(
                    "检查插件数据目录失败",
                    error,
                ));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AppError::conflict("插件数据路径不是普通目录"));
        }
    }
    Ok(Some(directory))
}

/// 遇到链接或目录一律拒绝，避免覆盖插件数据以外的文件。
fn state_mode(path: &Path) -> AppResult<CommitMode> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(CommitMode::CreateNew),
        Err(error) => Err(AppError::infrastructure_with_source(
            "检查插件状态失败",
            error,
        )),
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_file() => {
            Ok(CommitMode::ReplaceExisting)
        }
        Ok(_) => Err(AppError::conflict("插件状态不是普通文件")),
    }
}

#[async_trait]
impl PluginStateRepository for ConfigurationStore {
    /// 读取损坏或超限文件时保留原件，不能把它当成首次运行。
    async fn get(
        &self,
        plugin: &PluginId,
        cancellation: CancellationToken,
    ) -> AppResult<Option<Value>> {
        let store = self.clone();
        let plugin = plugin.clone();
        tokio::task::spawn_blocking(move || {
            ensure_active(&cancellation)?;
            let _lock =
                store.inner.storage.lock().map_err(|error| {
                    AppError::infrastructure_with_source("无法锁定插件状态", error)
                })?;
            ensure_active(&cancellation)?;
            let Some(directory) = data_directory(&store.inner.root, &plugin, false)? else {
                return Ok(None);
            };
            let path = directory.join("state.json");
            if matches!(state_mode(&path)?, CommitMode::CreateNew) {
                return Ok(None);
            }
            let metadata = fs::metadata(&path)
                .map_err(|error| AppError::infrastructure_with_source("检查插件状态失败", error))?;
            if metadata.len() > MAX_PLUGIN_STATE_BYTES as u64 {
                return Err(AppError::configuration_corrupt("插件状态超出上限"));
            }
            let bytes = fs::read(path)
                .map_err(|error| AppError::infrastructure_with_source("读取插件状态失败", error))?;
            if bytes.len() > MAX_PLUGIN_STATE_BYTES {
                return Err(AppError::configuration_corrupt("插件状态超出上限"));
            }
            ensure_active(&cancellation)?;
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| AppError::configuration_corrupt("插件状态"))
        })
        .await
        .map_err(|error| AppError::infrastructure_with_source("读取插件状态任务失败", error))?
    }

    /// 在真实提交前重新检查取消和目标类型，提交结果未知时保留暂存材料。
    async fn set(
        &self,
        plugin: &PluginId,
        state: Value,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        let store = self.clone();
        let plugin = plugin.clone();
        tokio::task::spawn_blocking(move || {
            ensure_active(&cancellation)?;
            let bytes =
                serde_json::to_vec(&state).map_err(|_| AppError::invalid("插件状态不能编码"))?;
            if bytes.len() > MAX_PLUGIN_STATE_BYTES {
                return Err(AppError::invalid("插件状态超过 1 MiB"));
            }
            let _lock =
                store.inner.storage.lock().map_err(|error| {
                    AppError::infrastructure_with_source("无法锁定插件状态", error)
                })?;
            store.ensure_writable()?;
            store.ensure_configuration_transactions_absent()?;
            ensure_active(&cancellation)?;
            let directory = data_directory(&store.inner.root, &plugin, true)?
                .ok_or_else(|| AppError::infrastructure("插件数据目录不可用"))?;
            let path = directory.join("state.json");
            state_mode(&path)?;
            let relative = Path::new("plugins")
                .join("data")
                .join(plugin.as_str())
                .join("state.json");
            super::super::atomic_replace(&store.inner.root, &relative, &bytes, || {
                ensure_active(&cancellation)?;
                state_mode(&path)
            })
        })
        .await
        .map_err(|error| {
            AppError::outcome_unknown_with_source("插件状态结果未知，请读回核对", error)
        })?
    }

    /// 显式删除整个数据目录；部分删除或目录同步失败均保留未知结果，不能宣称已完成。
    async fn delete_data(
        &self,
        plugin: &PluginId,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        let store = self.clone();
        let plugin = plugin.clone();
        tokio::task::spawn_blocking(move || {
            ensure_active(&cancellation)?;
            let _lock =
                store.inner.storage.lock().map_err(|error| {
                    AppError::infrastructure_with_source("无法锁定插件数据", error)
                })?;
            store.ensure_writable()?;
            store.ensure_configuration_transactions_absent()?;
            let Some(directory) = data_directory(&store.inner.root, &plugin, false)? else {
                return Ok(());
            };
            ensure_active(&cancellation)?;
            fs::remove_dir_all(&directory).map_err(|error| {
                AppError::outcome_unknown_with_source(
                    "插件数据删除可能部分完成，请检查后继续",
                    error,
                )
            })?;
            #[cfg(unix)]
            if let Some(parent) = directory.parent() {
                fs::File::open(parent)
                    .and_then(|file| file.sync_all())
                    .map_err(|error| {
                        AppError::outcome_unknown_with_source(
                            "插件数据删除结果未知，请检查后继续",
                            error,
                        )
                    })?;
            }
            Ok(())
        })
        .await
        .map_err(|error| {
            AppError::outcome_unknown_with_source("插件数据删除结果未知，请检查后继续", error)
        })?
    }
}
