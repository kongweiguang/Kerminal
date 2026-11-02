//! 设置热更新与消费者行为的隔离验证，不访问真实 SSH 主机或系统通知。
//!
//! @author kongweiguang

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppResult,
    platform::{
        DesktopNotificationEvent, DesktopNotificationService, NotificationDisposition,
        NotificationPayload, NotificationPort, WindowVisibility,
    },
    preferences::{DesktopNotificationSettings, SftpPerformanceSettings},
    transfers::{
        SftpTransferPolicySnapshot, SftpTransferPolicyStore, TransferBackend,
        TransferExecutionContext, TransferExecutionError, TransferExecutionResult,
        TransferQueueOptions, TransferService,
    },
};
use kerminal_domain::{
    files::{ConflictPolicy, FileEndpoint, TransferKind, TransferRequest},
    identity::HostId,
};
use tokio::{runtime::Handle, sync::mpsc, time::timeout};

struct PolicyCaptureBackend {
    started: mpsc::UnboundedSender<SftpTransferPolicySnapshot>,
}

#[async_trait]
impl TransferBackend for PolicyCaptureBackend {
    /// 只接受测试显式构造的远端源，避免此替身意外承担容器或纯本机传输。
    ///
    /// @author kongweiguang
    fn supports(&self, request: &TransferRequest) -> bool {
        matches!(request.source, FileEndpoint::Remote { .. })
    }

    /// 测试真实队列对 SFTP 策略的快照和准入，而不是另一套模拟限流。
    ///
    /// @author kongweiguang
    fn uses_sftp_policy(&self, _request: &TransferRequest) -> bool {
        true
    }

    /// 记录队列下发的冻结策略并等待取消，使测试能够观察并发上限。
    ///
    /// @author kongweiguang
    async fn execute(
        &self,
        context: TransferExecutionContext,
    ) -> Result<TransferExecutionResult, TransferExecutionError> {
        self.started
            .send(context.sftp_policy.expect("SFTP task has a policy"))
            .expect("test keeps the receiver alive");
        context.cancellation.cancelled().await;
        Err(TransferExecutionError::Cancelled {
            checkpoint: None,
            actual_destination: Some(context.request.destination),
        })
    }
}

/// 生成彼此独立的保存主机和目标，不触碰本机或远端文件系统。
///
/// @author kongweiguang
fn request(name: &str) -> TransferRequest {
    TransferRequest {
        source: FileEndpoint::remote(HostId::new(), &format!("/{name}.bin")).unwrap(),
        destination: FileEndpoint::Local {
            path: PathBuf::from(format!("{name}.bin")),
        },
        kind: TransferKind::File,
        conflict_policy: ConflictPolicy::Overwrite,
        idle_timeout_seconds: None,
    }
}

/// 新任务应携带新设置，旧任务保留入队参数；新全局并发对真实队列准入生效。
///
/// @author kongweiguang
#[tokio::test]
async fn sftp_policy_updates_reach_new_queued_tasks_without_changing_existing_tasks()
-> AppResult<()> {
    let old = SftpPerformanceSettings {
        global_transfers: 1,
        host_transfers: 1,
        pipeline_depth: 4,
        packet_bytes: 32 * 1024,
        idle_timeout_seconds: 90,
    };
    let new = SftpPerformanceSettings {
        global_transfers: 2,
        host_transfers: 1,
        pipeline_depth: 16,
        packet_bytes: 128 * 1024,
        idle_timeout_seconds: 240,
    };
    let store = SftpTransferPolicyStore::new(old)?;
    let (sender, mut started) = mpsc::unbounded_channel();
    let service = TransferService::new_with_sftp_policy(
        vec![Arc::new(PolicyCaptureBackend { started: sender })],
        Handle::current(),
        TransferQueueOptions::default(),
        store.clone(),
    )?;

    let first = service.enqueue(request("first")).await?;
    let old_snapshot = timeout(Duration::from_secs(2), started.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old_snapshot.global_transfers(), 1);
    assert_eq!(old_snapshot.host_transfers(), 1);
    assert_eq!(old_snapshot.pipeline_depth(), 4);
    assert_eq!(old_snapshot.packet_bytes(), 32 * 1024);
    assert_eq!(old_snapshot.idle_timeout(), Duration::from_secs(90));

    store.update(new)?;
    let second = service.enqueue(request("second")).await?;
    let new_snapshot = timeout(Duration::from_secs(2), started.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(new_snapshot.global_transfers(), 2);
    assert_eq!(new_snapshot.host_transfers(), 1);
    assert_eq!(new_snapshot.pipeline_depth(), 16);
    assert_eq!(new_snapshot.packet_bytes(), 128 * 1024);
    assert_eq!(new_snapshot.idle_timeout(), Duration::from_secs(240));
    assert_eq!(
        service.get(&first.id).await?.effective_idle_timeout_seconds,
        Some(90)
    );
    assert_eq!(
        service
            .get(&second.id)
            .await?
            .effective_idle_timeout_seconds,
        Some(240)
    );

    let third = service.enqueue(request("third")).await?;
    assert!(
        timeout(Duration::from_millis(100), started.recv())
            .await
            .is_err()
    );
    service.cancel(&first.id).await?;
    let third_snapshot = timeout(Duration::from_secs(2), started.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(third_snapshot, new_snapshot);
    service.cancel(&second.id).await?;
    service.cancel(&third.id).await?;
    service.shutdown().await?;
    Ok(())
}

#[derive(Default)]
struct CaptureNotifications(Mutex<Vec<NotificationPayload>>);

#[async_trait]
impl NotificationPort for CaptureNotifications {
    /// 保留投递请求用于验证过滤结果，不假装操作系统已展示通知。
    ///
    /// @author kongweiguang
    async fn send(&self, payload: NotificationPayload) -> AppResult<()> {
        self.0.lock().unwrap().push(payload);
        Ok(())
    }
}

/// 后台、重要事件、耗时和节流条件必须作用于同一个可热更新的通知服务。
///
/// @author kongweiguang
#[tokio::test]
async fn notification_settings_filter_real_events_after_runtime_updates() -> AppResult<()> {
    let port = Arc::new(CaptureNotifications::default());
    let service = DesktopNotificationService::new(
        port.clone(),
        DesktopNotificationSettings {
            enabled: true,
            background_only: true,
            important_only: false,
            min_duration_ms: 10_000,
            throttle_ms: 30_000,
        },
    );
    let completed = DesktopNotificationEvent::SftpBatchCompleted {
        host_label: None,
        total_count: 1,
        succeeded_count: Some(1),
        failed_count: Some(0),
        duration: Duration::from_secs(30),
        importance: None,
    };
    let failed = DesktopNotificationEvent::SftpTransferFailed {
        host_label: None,
        failed_count: 1,
        importance: None,
    };
    assert_eq!(
        service
            .notify(completed.clone(), WindowVisibility::Foreground)
            .await?,
        NotificationDisposition::BackgroundOnly
    );
    assert_eq!(
        service
            .notify(failed.clone(), WindowVisibility::Foreground)
            .await?,
        NotificationDisposition::BackgroundOnly
    );
    assert_eq!(
        service
            .notify(completed.clone(), WindowVisibility::Hidden)
            .await?,
        NotificationDisposition::Submitted
    );
    assert_eq!(
        service
            .notify(completed.clone(), WindowVisibility::Hidden)
            .await?,
        NotificationDisposition::Throttled
    );

    service.update_settings(DesktopNotificationSettings {
        enabled: true,
        background_only: false,
        important_only: true,
        min_duration_ms: 120_000,
        throttle_ms: 0,
    })?;
    assert_eq!(
        service
            .notify(completed.clone(), WindowVisibility::Foreground)
            .await?,
        NotificationDisposition::ImportantOnly
    );
    assert_eq!(
        service.notify(failed, WindowVisibility::Foreground).await?,
        NotificationDisposition::Submitted
    );
    assert_eq!(port.0.lock().unwrap().len(), 2);

    service.update_settings(DesktopNotificationSettings {
        enabled: true,
        background_only: false,
        important_only: false,
        min_duration_ms: 120_000,
        throttle_ms: 0,
    })?;
    assert_eq!(
        service
            .notify(completed.clone(), WindowVisibility::Foreground)
            .await?,
        NotificationDisposition::ForegroundShortEvent
    );
    service.update_settings(DesktopNotificationSettings {
        enabled: false,
        ..DesktopNotificationSettings::default()
    })?;
    assert_eq!(
        service
            .notify(completed, WindowVisibility::Minimized)
            .await?,
        NotificationDisposition::Disabled
    );
    Ok(())
}
