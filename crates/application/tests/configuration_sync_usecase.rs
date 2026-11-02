//! 设置页单次同步编排的隔离用例测试。
//!
//! @author kongweiguang

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    configuration_sync::{
        ConfigurationSyncOperationKind as Kind, ConfigurationSyncOperationResult as ResultDto,
        ConfigurationSyncOutcome as Outcome, ConfigurationSyncRepository, ConfigurationSyncService,
        ConfigurationSyncStatus, SyncPathStatus, SyncRepositoryState,
    },
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct SyncFixture {
    status: Arc<Mutex<ConfigurationSyncStatus>>,
    operations: Arc<Mutex<Vec<Kind>>>,
    pull_outcome: Outcome,
    authentication_failure: Arc<AtomicBool>,
}

#[async_trait]
impl ConfigurationSyncRepository for SyncFixture {
    async fn status(&self) -> AppResult<ConfigurationSyncStatus> {
        Ok(self.status.lock().unwrap().clone())
    }

    async fn fetch(&self, _: CancellationToken) -> AppResult<ResultDto> {
        Ok(result(Kind::Fetch, Outcome::Succeeded))
    }

    /// 允许测试注入认证失败，同时记录每次变基请求以检查重试不会重复提交。
    ///
    /// @author kongweiguang
    async fn pull(&self, _: CancellationToken) -> AppResult<ResultDto> {
        self.operations.lock().unwrap().push(Kind::Pull);
        if self.authentication_failure.load(Ordering::Acquire) {
            return Err(AppError::authentication_failed());
        }
        let outcome = self.pull_outcome;
        if outcome == Outcome::Succeeded {
            let mut status = self.status.lock().unwrap();
            status.behind = Some(0);
        }
        Ok(result(Kind::Pull, outcome))
    }

    async fn commit(&self, _: CancellationToken) -> AppResult<ResultDto> {
        self.operations.lock().unwrap().push(Kind::Commit);
        let mut status = self.status.lock().unwrap();
        status.local_changes.clear();
        status.ahead = Some(status.ahead.unwrap_or_default() + 1);
        Ok(result(Kind::Commit, Outcome::Succeeded))
    }

    async fn push(&self, _: CancellationToken) -> AppResult<ResultDto> {
        self.operations.lock().unwrap().push(Kind::Push);
        self.status.lock().unwrap().ahead = Some(0);
        Ok(result(Kind::Push, Outcome::Succeeded))
    }

    async fn continue_rebase(&self, _: CancellationToken) -> AppResult<ResultDto> {
        unreachable!("synchronize does not resolve user conflicts")
    }

    async fn abort_rebase(&self, _: CancellationToken) -> AppResult<ResultDto> {
        unreachable!("synchronize does not resolve user conflicts")
    }
}

/// Fixture 状态只承载同步工作流需要的路径分类和 ahead/behind 计数。
fn status(
    repository: SyncRepositoryState,
    changes: Vec<SyncPathStatus>,
) -> ConfigurationSyncStatus {
    ConfigurationSyncStatus {
        git_available: true,
        repository,
        writable: true,
        branch: Some("main".to_owned()),
        upstream: (repository == SyncRepositoryState::Ready).then(|| "origin/main".to_owned()),
        ahead: Some(0),
        behind: Some(1),
        local_changes: changes,
        protected_changes: Vec::new(),
        conflicts: Vec::new(),
        rebase_in_progress: false,
        missing_ignore_rules: Vec::new(),
    }
}

/// 操作回执摘要只用于断言服务按预期停止或继续后续阶段。
fn result(kind: Kind, outcome: Outcome) -> ResultDto {
    ResultDto {
        kind,
        outcome,
        summary: format!("{kind:?}"),
        affected_paths: Vec::new(),
        commit_hash: None,
    }
}

/// 完整同步先提交脏工作区，再变基并推送，同时保留每个阶段的顺序。
#[tokio::test]
async fn synchronize_commits_then_pulls_and_pushes() {
    let operations = Arc::new(Mutex::new(Vec::new()));
    let repository = SyncFixture {
        status: Arc::new(Mutex::new(status(
            SyncRepositoryState::Ready,
            vec![SyncPathStatus {
                path: "settings.toml".to_owned(),
                staged: false,
                unstaged: true,
                untracked: false,
                conflicted: false,
            }],
        ))),
        operations: operations.clone(),
        pull_outcome: Outcome::Succeeded,
        authentication_failure: Arc::new(AtomicBool::new(false)),
    };
    let service = ConfigurationSyncService::new(Arc::new(repository));

    let result = service.synchronize(CancellationToken::new()).await.unwrap();

    assert_eq!(result.outcome, Outcome::Succeeded);
    assert_eq!(
        *operations.lock().unwrap(),
        vec![Kind::Commit, Kind::Pull, Kind::Push]
    );
    assert_eq!(result.stages.len(), 3);
}

/// 变基冲突必须中止后续 push，已提交的本地阶段继续留在回执中。
#[tokio::test]
async fn synchronize_stops_after_pull_conflict() {
    let operations = Arc::new(Mutex::new(Vec::new()));
    let repository = SyncFixture {
        status: Arc::new(Mutex::new(status(SyncRepositoryState::Ready, Vec::new()))),
        operations: operations.clone(),
        pull_outcome: Outcome::Conflict,
        authentication_failure: Arc::new(AtomicBool::new(false)),
    };
    let service = ConfigurationSyncService::new(Arc::new(repository));

    let result = service.synchronize(CancellationToken::new()).await.unwrap();

    assert_eq!(result.outcome, Outcome::Conflict);
    assert_eq!(*operations.lock().unwrap(), vec![Kind::Pull]);
    assert_eq!(result.stages.len(), 1);
}

/// 缺失上游时允许本地提交，整体状态仍须提示远端同步未完成。
///
/// @author kongweiguang
#[tokio::test]
async fn synchronize_without_upstream_keeps_local_commit_only() {
    let operations = Arc::new(Mutex::new(Vec::new()));
    let repository = SyncFixture {
        status: Arc::new(Mutex::new(status(
            SyncRepositoryState::NoUpstream,
            vec![SyncPathStatus {
                path: "settings.toml".to_owned(),
                staged: false,
                unstaged: true,
                untracked: false,
                conflicted: false,
            }],
        ))),
        operations: operations.clone(),
        pull_outcome: Outcome::Succeeded,
        authentication_failure: Arc::new(AtomicBool::new(false)),
    };
    let service = ConfigurationSyncService::new(Arc::new(repository));

    let result = service.synchronize(CancellationToken::new()).await.unwrap();

    assert_eq!(result.outcome, Outcome::Blocked);
    assert_eq!(*operations.lock().unwrap(), vec![Kind::Commit]);
    assert!(result.summary.contains("没有上游"));
}

/// 空工作区且没有上游时仍需提示配置上游，不能误报远端同步完成。
///
/// @author kongweiguang
#[tokio::test]
async fn synchronize_without_upstream_and_changes_reports_no_changes() {
    let operations = Arc::new(Mutex::new(Vec::new()));
    let repository = SyncFixture {
        status: Arc::new(Mutex::new(status(
            SyncRepositoryState::NoUpstream,
            Vec::new(),
        ))),
        operations: operations.clone(),
        pull_outcome: Outcome::Succeeded,
        authentication_failure: Arc::new(AtomicBool::new(false)),
    };
    let service = ConfigurationSyncService::new(Arc::new(repository));

    let result = service.synchronize(CancellationToken::new()).await.unwrap();

    assert_eq!(result.outcome, Outcome::Blocked);
    assert!(operations.lock().unwrap().is_empty());
    assert!(result.summary.contains("没有需要提交"));
}

/// 认证失败要保留已提交回执；修复凭据后的显式重试不得重复创建本地提交。
///
/// @author kongweiguang
#[tokio::test]
async fn authentication_failure_preserves_local_commit_without_repeating_it() {
    let operations = Arc::new(Mutex::new(Vec::new()));
    let authentication_failure = Arc::new(AtomicBool::new(true));
    let repository = SyncFixture {
        status: Arc::new(Mutex::new(status(
            SyncRepositoryState::Ready,
            vec![SyncPathStatus {
                path: "settings.toml".to_owned(),
                staged: false,
                unstaged: true,
                untracked: false,
                conflicted: false,
            }],
        ))),
        operations: operations.clone(),
        pull_outcome: Outcome::Succeeded,
        authentication_failure: authentication_failure.clone(),
    };
    let service = ConfigurationSyncService::new(Arc::new(repository));

    let first = service.synchronize(CancellationToken::new()).await.unwrap();
    assert_eq!(first.outcome, Outcome::Blocked);
    assert_eq!(first.stages.len(), 1);
    assert_eq!(first.stages[0].kind, Kind::Commit);
    assert!(first.summary.contains("认证失败"));

    authentication_failure.store(false, Ordering::Release);
    let retried = service.synchronize(CancellationToken::new()).await.unwrap();
    assert_eq!(retried.outcome, Outcome::Succeeded);
    assert_eq!(
        *operations.lock().unwrap(),
        vec![Kind::Commit, Kind::Pull, Kind::Pull, Kind::Push]
    );
}
