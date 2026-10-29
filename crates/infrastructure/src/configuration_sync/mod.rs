//! 配置 Git 的显式获取、变基、本地提交和推送适配器。
//!
//! @author kongweiguang

mod actions;
mod git;
mod gitignore;
mod protected;
mod runner;
mod status;

use std::path::PathBuf;

use async_trait::async_trait;
use kerminal_application::configuration_sync::{
    ConfigurationSyncOperationResult, ConfigurationSyncRepository, ConfigurationSyncStatus,
};
use kerminal_application::operations::OperationsCommandExecutor;
use kerminal_application::{AppError, AppResult};
use kerminal_domain::preferences::{ConfigurationMode, ConfigurationRecoveryState};
use tokio_util::sync::CancellationToken;

use crate::{configuration::ConfigurationStore, vault::VaultStore};

/// Git 操作绑定到桌面配置存储和 vault 事务共用的数据根与存储锁。
#[derive(Clone)]
pub struct GitConfigurationSyncRepository {
    configuration: ConfigurationStore,
    vault: VaultStore,
    root: PathBuf,
    git_runtime: std::sync::Arc<runner::GitCommandRuntime>,
}

impl GitConfigurationSyncRepository {
    /// 复用桌面已初始化的存储；数据根不一致时在启动 Git 前拒绝装配。
    pub fn new(
        configuration: ConfigurationStore,
        vault: VaultStore,
        executor: std::sync::Arc<dyn OperationsCommandExecutor>,
        runtime: tokio::runtime::Handle,
    ) -> AppResult<Self> {
        let root = configuration.root().to_path_buf();
        if root != vault.root() {
            return Err(AppError::invalid("Git 同步与配置存储必须使用同一数据目录"));
        }
        Ok(Self {
            configuration,
            vault,
            root,
            git_runtime: std::sync::Arc::new(runner::GitCommandRuntime::new(executor, runtime)),
        })
    }

    /// 读取启动时确定的恢复状态，避免后续 UI 动作绕过只读模式。
    pub(super) fn recovery_state(&self) -> ConfigurationRecoveryState {
        self.configuration.recovery_state()
    }

    /// 配置以恢复模式启动时拒绝检出、提交和推送等写操作。
    pub(super) fn ensure_writable(&self) -> AppResult<()> {
        if self.recovery_state().mode == ConfigurationMode::ReadWrite {
            Ok(())
        } else {
            Err(AppError::conflict(
                "配置处于只读恢复模式，Git 同步写操作已暂停",
            ))
        }
    }

    /// 快照或变更本地 Git 工作树前恢复 vault 并拒绝尚未清理的片段导入事务。
    pub(super) fn lock_for_local_operation(&self) -> AppResult<crate::storage::StorageLock<'_>> {
        let lock = self.vault.lock_for_sync().map_err(|source| {
            AppError::infrastructure_with_source("确认本地配置存储状态失败", source)
        })?;
        protected::ensure_no_pending_snippet_import(&self.root)?;
        Ok(lock)
    }

    /// 以组合根提供的绝对截止时间回收唯一 Git worker；超时会在共享 runtime 中保留 owner。
    pub async fn shutdown_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.git_runtime.shutdown_until(deadline).await
    }

    /// 在同一 Git gate 下运行阻塞工作；清理待确认时返回未知状态但让原 blocking owner 继续持锁。
    pub(super) async fn run_blocking<T, F>(
        self,
        cancellation: CancellationToken,
        context: &'static str,
        work: F,
    ) -> AppResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&GitConfigurationSyncRepository, &CancellationToken) -> AppResult<T>
            + Send
            + 'static,
    {
        let permit = self
            .git_runtime
            .begin_operation(cancellation.clone())
            .await
            .map_err(|error| match error {
                runner::GitOperationStartError::Busy => {
                    AppError::conflict("已有 Git 同步操作正在执行")
                }
                runner::GitOperationStartError::PendingCleanup => {
                    AppError::outcome_unknown("Git 上次操作仍在等待子进程清理确认")
                }
                runner::GitOperationStartError::ShuttingDown => {
                    AppError::cancelled("Git 同步服务正在关闭")
                }
                runner::GitOperationStartError::PreviousWorkerFailed(source) => {
                    AppError::infrastructure_with_source("上一次 Git worker 收尾失败", source)
                }
            })?;
        let repository = self.clone();
        let (result_sender, mut result_receiver) = tokio::sync::oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            let result = work(&repository, &cancellation);
            let _ = result_sender.send(result);
        });
        self.git_runtime.track_worker(permit, task);
        let mut state = self.git_runtime.subscribe_state();
        loop {
            if *state.borrow_and_update() == runner::GitWorkerState::PendingCleanup {
                return Err(AppError::outcome_unknown(
                    "Git 操作仍在等待原子进程清理确认",
                ));
            }
            tokio::select! {
                result = &mut result_receiver => {
                    self.git_runtime
                        .join_worker_until(tokio::time::Instant::now() + runner::INITIAL_CLEANUP_BUDGET)
                        .await
                        .map_err(|error| match error {
                            runner::GitWorkerJoinError::Pending => {
                                AppError::outcome_unknown("Git 结果已产生，但 worker 尚未完成退出")
                            }
                            runner::GitWorkerJoinError::Failed(source) => {
                                AppError::infrastructure_with_source(context, source)
                            }
                        })?;
                    return result.unwrap_or_else(|_| Err(AppError::infrastructure(context)));
                }
                changed = state.changed() => {
                    changed.map_err(|_| AppError::infrastructure("Git worker 状态订阅已关闭"))?;
                    if *state.borrow_and_update() == runner::GitWorkerState::PendingCleanup {
                        return Err(AppError::outcome_unknown(
                            "Git 操作仍在等待原子进程清理确认",
                        ));
                    }
                }
            }
        }
    }
}

#[async_trait]
impl ConfigurationSyncRepository for GitConfigurationSyncRepository {
    /// 读取仓库与受保护路径状态，不启动网络同步。
    async fn status(&self) -> AppResult<ConfigurationSyncStatus> {
        status::status(self.clone()).await
    }

    /// 从已装配的 ConfigurationStore 根目录生成真实展示路径，并固定 key 文件相对位置。
    async fn workspace_info(
        &self,
    ) -> AppResult<Option<kerminal_application::configuration_sync::ConfigurationWorkspaceInfo>>
    {
        Ok(Some(
            kerminal_application::configuration_sync::ConfigurationWorkspaceInfo {
                workspace_root: self.root.to_string_lossy().into_owned(),
                vault_key_path: self
                    .root
                    .join("secrets")
                    .join("vault-key.toml")
                    .to_string_lossy()
                    .into_owned(),
            },
        ))
    }

    /// 只获取远端引用；fetch 子进程不持共享存储锁。
    async fn fetch(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult> {
        actions::fetch(self.clone(), cancellation).await
    }

    /// 检查本地和远端保护路径后，在共享锁下变基已获取的上游变更。
    async fn pull(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult> {
        actions::pull(self.clone(), cancellation).await
    }

    /// 提交已枚举的可同步变更，并保留无关暂存项和受保护文件。
    async fn commit(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult> {
        actions::commit(self.clone(), cancellation).await
    }

    /// 将预检过的提交推送到配置的上游，网络 I/O 期间不持本地存储锁。
    async fn push(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult> {
        actions::push(self.clone(), cancellation).await
    }

    /// 只有冲突和受保护路径检查通过后才继续用户处理过的变基。
    async fn continue_rebase(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult> {
        actions::continue_rebase(self.clone(), cancellation).await
    }

    /// 仅在恢复期间没有本地保护文件变更时中止变基。
    async fn abort_rebase(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult> {
        actions::abort_rebase(self.clone(), cancellation).await
    }
}
