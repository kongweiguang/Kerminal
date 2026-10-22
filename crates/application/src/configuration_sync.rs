//! 配置 Git 同步用例与桌面界面使用的安全结果类型。
//!
//! @author kongweiguang

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::AppResult;

/// 仓库状态与操作错误分离，让尚未配置 Git 的数据目录仍能显示配置页面。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncRepositoryState {
    Missing,
    Ready,
    Detached,
    NoUpstream,
    /// 同一仓库仍有 Git 子进程清理在途；普通同步动作须等待原 owner 确认回收。
    PendingBusy,
}

/// 一个经过脱敏的根目录相对路径及冲突恢复所需 Git 状态。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncPathStatus {
    pub path: String,
    pub staged: bool,
    pub unstaged: bool,
    pub untracked: bool,
    pub conflicted: bool,
}

/// 当前仓库和本地变更摘要；不包含远端 URL 或文件内容。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationSyncStatus {
    pub git_available: bool,
    pub repository: SyncRepositoryState,
    pub writable: bool,
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: Option<u64>,
    pub behind: Option<u64>,
    pub local_changes: Vec<SyncPathStatus>,
    pub protected_changes: Vec<SyncPathStatus>,
    pub conflicts: Vec<String>,
    pub rebase_in_progress: bool,
    pub missing_ignore_rules: Vec<String>,
}

/// 用户显式触发的 Git 动作；fetch 不会隐含检出或推送。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationSyncOperationKind {
    Fetch,
    Pull,
    Commit,
    Push,
    ContinueRebase,
    AbortRebase,
}

/// 操作结果分类，将冲突、取消和未知结果与成功明确区分。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationSyncOutcome {
    Succeeded,
    NoChanges,
    Conflict,
    Cancelled,
    Blocked,
    OutcomeUnknown,
}

/// 单次操作的安全摘要；本地提交和远端推送始终是不同结果。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationSyncOperationResult {
    pub kind: ConfigurationSyncOperationKind,
    pub outcome: ConfigurationSyncOutcome,
    pub summary: String,
    pub affected_paths: Vec<String>,
    pub commit_hash: Option<String>,
}

/// 基础设施端口；所有变更动作都需显式调用并接收调用方的取消令牌。
#[async_trait]
pub trait ConfigurationSyncRepository: Send + Sync + 'static {
    /// 只读取仓库状态，不修改文件、引用或远端。
    async fn status(&self) -> AppResult<ConfigurationSyncStatus>;

    /// 仅获取远端引用，不检出或合并远端内容。
    async fn fetch(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult>;

    /// 检查受保护路径后，将本地提交变基到已获取的上游内容。
    async fn pull(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult>;

    /// 仅提交已枚举的非保护路径变更，不带入无关的暂存路径。
    async fn commit(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult>;

    /// 确认待推送提交未改动受保护路径后再推送。
    async fn push(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult>;

    /// 用户解决冲突后继续变基，不弹出编辑器，也不隐藏剩余冲突。
    async fn continue_rebase(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult>;

    /// 仅在恢复期间未改动受保护路径时中止变基。
    async fn abort_rebase(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult>;
}

/// 桌面 UI 共用的应用服务句柄，不包含 Git 进程或文件系统策略。
#[derive(Clone)]
pub struct ConfigurationSyncService {
    repository: Arc<dyn ConfigurationSyncRepository>,
}

impl ConfigurationSyncService {
    /// 注入统一仓储，使各 UI 入口共用数据根、保护路径规则和恢复策略。
    pub fn new(repository: Arc<dyn ConfigurationSyncRepository>) -> Self {
        Self { repository }
    }

    /// 读取仓库状态，不触发后台同步。
    pub async fn status(&self) -> AppResult<ConfigurationSyncStatus> {
        self.repository.status().await
    }

    /// 单独获取远端引用；不会检出本地文件。
    pub async fn fetch(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult> {
        self.repository.fetch(cancellation).await
    }

    /// 应用已获取的上游提交；需用户处理时保留可诊断的变基状态。
    pub async fn pull(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult> {
        self.repository.pull(cancellation).await
    }

    /// 仅从可同步路径创建本地提交，不代表变更已推送。
    pub async fn commit(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult> {
        self.repository.commit(cancellation).await
    }

    /// 将验证过的本地提交推送到配置的上游，并单独报告远端结果。
    pub async fn push(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult> {
        self.repository.push(cancellation).await
    }

    /// 用户解决报告的全部冲突后继续当前变基。
    pub async fn continue_rebase(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult> {
        self.repository.continue_rebase(cancellation).await
    }

    /// 用户放弃冲突处理时恢复变基前的分支状态。
    pub async fn abort_rebase(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult> {
        self.repository.abort_rebase(cancellation).await
    }
}
