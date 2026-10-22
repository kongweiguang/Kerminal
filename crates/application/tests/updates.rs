// @author kongweiguang
//! 更新用例只验证端口边界，不执行网络、磁盘安装或平台重启。

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use kerminal_application::updates::{UpdateRecoveryId, UpdateRecoveryRecord, UpdateRecoveryState};
use kerminal_application::{
    AppError, AppResult, DownloadedUpdate, DownloadedUpdateId, UpdateCandidate, UpdateCandidateId,
    UpdateInstallDisposition, UpdateInstallResult, UpdatePort, UpdateProgress, UpdateService,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct FakeUpdatePort {
    checks: AtomicUsize,
    downloads: AtomicUsize,
    installs: AtomicUsize,
    discards: AtomicUsize,
    check_error: bool,
    recoveries: Mutex<Vec<UpdateRecoveryRecord>>,
    acknowledgements: AtomicUsize,
}

#[async_trait]
impl UpdatePort for FakeUpdatePort {
    /// 记录调用并返回固定候选，避免测试依赖真实 release feed。
    async fn check(&self, cancel: CancellationToken) -> AppResult<Option<UpdateCandidate>> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        if self.check_error {
            return Err(AppError::infrastructure("测试检查失败"));
        }
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("测试检查已取消"));
        }
        Ok(Some(UpdateCandidate {
            id: UpdateCandidateId::new(),
            version: "0.4.1".to_owned(),
            notes: Some("test notes".to_owned()),
            published_at: None,
        }))
    }

    /// 返回隔离的已下载身份并发布最新进度快照。
    async fn download(
        &self,
        _candidate_id: UpdateCandidateId,
        cancel: CancellationToken,
        progress: watch::Sender<UpdateProgress>,
    ) -> AppResult<DownloadedUpdate> {
        self.downloads.fetch_add(1, Ordering::SeqCst);
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("测试下载已取消"));
        }
        progress.send_replace(UpdateProgress {
            received_bytes: 17,
            total_bytes: Some(42),
        });
        Ok(DownloadedUpdate {
            id: DownloadedUpdateId::new(),
            version: "0.4.1".to_owned(),
        })
    }

    /// 记录已验签句柄的安装委托，不在单元测试中重启或替换应用文件。
    async fn install(&self, _update_id: DownloadedUpdateId) -> UpdateInstallResult {
        self.installs.fetch_add(1, Ordering::SeqCst);
        UpdateInstallResult::Installed(UpdateInstallDisposition::RestartRequired)
    }

    /// 记录显式丢弃，验证应用层不会把 cleanup 静默跳过。
    async fn discard(&self, _update_id: DownloadedUpdateId) -> AppResult<()> {
        self.discards.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// 只返回恢复元数据，确保列出未知安装状态不会触发 SDK 或清理材料。
    async fn recoveries(&self) -> AppResult<Vec<UpdateRecoveryRecord>> {
        self.recoveries
            .lock()
            .map(|records| records.clone())
            .map_err(|_| AppError::infrastructure("测试恢复目录锁不可用"))
    }

    /// 模拟用户明确核实后的恢复记录清理，不将该确认映射为安装重试。
    async fn acknowledge_recovery(&self, recovery_id: UpdateRecoveryId) -> AppResult<()> {
        self.acknowledgements.fetch_add(1, Ordering::SeqCst);
        let mut records = self
            .recoveries
            .lock()
            .map_err(|_| AppError::infrastructure("测试恢复目录锁不可用"))?;
        let Some(index) = records.iter().position(|record| record.id == recovery_id) else {
            return Err(AppError::not_found("测试恢复记录不存在"));
        };
        records.remove(index);
        Ok(())
    }
}

#[tokio::test]
/// 预先取消时应在应用层短路，避免制造无意义的网络任务。
async fn cancelled_check_does_not_call_port() {
    let port = Arc::new(FakeUpdatePort::default());
    let service = UpdateService::new(port.clone());
    let cancel = CancellationToken::new();
    cancel.cancel();

    let result = service.check(cancel).await;

    assert!(matches!(result, Err(AppError::Cancelled { .. })));
    assert_eq!(port.checks.load(Ordering::SeqCst), 0);
}

#[tokio::test]
/// 失败和无更新使用不同结果，避免 UI 将网络故障显示成“已是最新版”。
async fn check_error_is_not_reported_as_no_update() {
    let port = Arc::new(FakeUpdatePort {
        check_error: true,
        ..FakeUpdatePort::default()
    });
    let service = UpdateService::new(port.clone());

    let result = service.check(CancellationToken::new()).await;

    assert!(matches!(result, Err(AppError::Infrastructure { .. })));
    assert_eq!(port.checks.load(Ordering::SeqCst), 1);
}

#[tokio::test]
/// watch 只保留最新进度，下载 ID 与候选 ID 通过不同 newtype 隔离。
async fn downloaded_progress_is_latest_value_and_install_uses_typed_handle() {
    let port = Arc::new(FakeUpdatePort::default());
    let service = UpdateService::new(port.clone());
    let candidate = service
        .check(CancellationToken::new())
        .await
        .expect("check succeeds")
        .expect("candidate exists");
    let (progress_sender, progress_receiver) = watch::channel(UpdateProgress::default());

    let downloaded = service
        .download(candidate.id, CancellationToken::new(), progress_sender)
        .await
        .expect("download succeeds");

    assert_eq!(downloaded.version, "0.4.1");
    assert_eq!(progress_receiver.borrow().received_bytes, 17);
    assert_eq!(progress_receiver.borrow().total_bytes, Some(42));
    assert!(matches!(
        service.install(downloaded.id).await,
        UpdateInstallResult::Installed(UpdateInstallDisposition::RestartRequired)
    ));
    service
        .discard(downloaded.id)
        .await
        .expect("discard delegates");
    assert_eq!(port.downloads.load(Ordering::SeqCst), 1);
    assert_eq!(port.installs.load(Ordering::SeqCst), 1);
    assert_eq!(port.discards.load(Ordering::SeqCst), 1);
}

#[tokio::test]
/// 已取消操作不得进入 updater 的阻塞 HTTP 下载实现。
async fn cancelled_download_does_not_call_port() {
    let port = Arc::new(FakeUpdatePort::default());
    let service = UpdateService::new(port.clone());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (progress_sender, _) = watch::channel(UpdateProgress::default());

    let result = service
        .download(UpdateCandidateId::new(), cancel, progress_sender)
        .await;

    assert!(matches!(result, Err(AppError::Cancelled { .. })));
    assert_eq!(port.downloads.load(Ordering::SeqCst), 0);
}

struct CancellationProbePort {
    started: tokio::sync::Notify,
    cancelled: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl UpdatePort for CancellationProbePort {
    /// 保持检查挂起直到用例发出取消，以验证取消传播和后台任务 join 顺序。
    async fn check(&self, cancel: CancellationToken) -> AppResult<Option<UpdateCandidate>> {
        self.started.notify_one();
        cancel.cancelled().await;
        self.cancelled.store(true, Ordering::SeqCst);
        Err(AppError::cancelled("测试检查已取消"))
    }

    /// 该探针只测试检查取消，下载路径明确返回测试错误。
    async fn download(
        &self,
        _candidate_id: UpdateCandidateId,
        _cancel: CancellationToken,
        _progress: watch::Sender<UpdateProgress>,
    ) -> AppResult<DownloadedUpdate> {
        Err(AppError::unsupported("探针不支持下载"))
    }

    /// 该探针不安装更新，避免测试携带未创建的下载句柄。
    async fn install(&self, _update_id: DownloadedUpdateId) -> UpdateInstallResult {
        UpdateInstallResult::NotStarted(AppError::unsupported("探针不支持安装"))
    }

    /// 该探针不持有暂存包，因此丢弃调用属于测试错误。
    async fn discard(&self, _update_id: DownloadedUpdateId) -> AppResult<()> {
        Err(AppError::unsupported("探针不支持丢弃"))
    }
}

#[tokio::test]
/// UI 的取消状态只有在端口观察到取消并结束后才返回，避免后台请求脱离 supervisor。
async fn cancellation_is_forwarded_and_joined_before_check_returns() {
    let port = Arc::new(CancellationProbePort {
        started: tokio::sync::Notify::new(),
        cancelled: std::sync::atomic::AtomicBool::new(false),
    });
    let service = UpdateService::new(port.clone());
    let user_cancel = CancellationToken::new();
    let check_cancel = user_cancel.clone();
    let check = tokio::spawn(async move {
        service
            .check_with_cancellation(check_cancel, CancellationToken::new())
            .await
    });

    port.started.notified().await;
    user_cancel.cancel();
    let result = check.await.expect("update check task joins");

    assert!(matches!(result, Err(AppError::Cancelled { .. })));
    assert!(port.cancelled.load(Ordering::SeqCst));
}

#[test]
/// 安装重试策略只接受能证明未提交或无副作用的结果，未知结果必须阻止盲目重放。
fn only_proven_no_effect_install_results_are_retryable() {
    assert!(UpdateInstallResult::NotStarted(AppError::conflict("测试未启动")).can_retry());
    assert!(
        UpdateInstallResult::FailedNoEffect(AppError::infrastructure("测试无副作用")).can_retry()
    );
    assert!(
        !UpdateInstallResult::OutcomeUnknown(AppError::outcome_unknown("测试未知")).can_retry()
    );
    assert!(!UpdateInstallResult::Installed(UpdateInstallDisposition::RestartRequired).can_retry());
}

#[tokio::test]
/// UI 只拿 application subscription；下载结束后仍能读取最后一个进度快照和正常关闭态。
async fn progress_subscription_hides_watch_and_returns_final_snapshot() {
    let port = Arc::new(FakeUpdatePort::default());
    let service = UpdateService::new(port);
    let candidate = service
        .check(CancellationToken::new())
        .await
        .expect("check succeeds")
        .expect("candidate exists");
    let (reporter, subscription) = UpdateService::progress_channel();

    let downloaded = service
        .download_with_cancellation(
            candidate.id,
            CancellationToken::new(),
            CancellationToken::new(),
            reporter,
        )
        .await
        .expect("download succeeds");
    let progress = subscription
        .next(CancellationToken::new())
        .await
        .expect("final progress is available");

    assert_eq!(downloaded.version, "0.4.1");
    assert_eq!(progress.received_bytes, 17);
    assert_eq!(progress.total_bytes, Some(42));
    assert_eq!(subscription.next(CancellationToken::new()).await, None);
}

#[tokio::test]
/// 恢复状态只在用户显式确认后清理，查询本身不能安装或删除记录。
async fn recovery_listing_is_read_only_and_acknowledgement_is_explicit() {
    let recovery_id = UpdateRecoveryId::new();
    let port = Arc::new(FakeUpdatePort {
        recoveries: Mutex::new(vec![UpdateRecoveryRecord {
            id: recovery_id,
            version: "0.4.2".to_owned(),
            target: "windows-x86_64".to_owned(),
            artifact_sha256: "a".repeat(64),
            state: UpdateRecoveryState::VersionMatchedAwaitingConfirmation,
            recovery_path: std::path::PathBuf::from("private/updates/recovery/example"),
        }]),
        ..FakeUpdatePort::default()
    });
    let service = UpdateService::new(port.clone());

    let visible_records = service
        .recoveries()
        .await
        .expect("recovery listing succeeds");

    assert_eq!(visible_records.len(), 1);
    assert_eq!(visible_records[0].id, recovery_id);
    assert_eq!(
        visible_records[0].state,
        UpdateRecoveryState::VersionMatchedAwaitingConfirmation
    );
    assert_eq!(
        service
            .recoveries()
            .await
            .expect("second read succeeds")
            .len(),
        1
    );
    assert_eq!(port.installs.load(Ordering::SeqCst), 0);
    assert_eq!(port.acknowledgements.load(Ordering::SeqCst), 0);

    service
        .acknowledge_recovery(recovery_id)
        .await
        .expect("explicit acknowledgement clears recovery metadata");

    assert!(
        service
            .recoveries()
            .await
            .expect("recovery list refreshes")
            .is_empty()
    );
    assert_eq!(port.installs.load(Ordering::SeqCst), 0);
    assert_eq!(port.acknowledgements.load(Ordering::SeqCst), 1);
}
