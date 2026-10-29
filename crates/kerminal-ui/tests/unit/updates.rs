// @author kongweiguang
//! 更新界面的纯格式投影测试，不构造窗口或触发任何更新副作用。

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context as TaskContext, Poll, Waker},
};

use kerminal_application::updates::{
    DownloadedUpdate, DownloadedUpdateId, UpdateCandidate, UpdateCandidateId, UpdateInstallResult,
    UpdateProgress, UpdateRecoveryId, UpdateRecoveryRecord, UpdateRecoveryState,
};
use kerminal_application::{
    AppError, AppResult,
    platform::{
        DesktopNotificationService, NotificationDisposition, NotificationPayload, NotificationPort,
        WindowVisibility,
    },
    preferences::settings::DesktopNotificationSettings,
};

use super::{
    SettingsInstallStep, UpdatePanelState,
    about::{APP_VERSION, LICENSE_SPDX, LICENSE_TEXT, PROJECT_REPOSITORY, repository_url},
    can_acknowledge_recovery, can_check_for_updates, can_exit_after_install_failure,
    can_retry_host_install, format_progress, listener_recovery_failure_matches, localized,
    notification_candidate_for_check, notify_update_available, recovery_state_label,
    restart_failure_matches,
    settings::{idle_update_status_label, settings_check_allowed},
    settings_install_step, take_check_operation, take_settings_auto_install_after_download,
    update_notification_diagnostic,
};

#[derive(Default)]
struct FakeNotificationPort {
    calls: AtomicUsize,
    payloads: Mutex<Vec<NotificationPayload>>,
    fail: bool,
}

impl NotificationPort for FakeNotificationPort {
    /// 此 fake port 立即完成 future，使测试验证真实 service 策略而不启动 OS 或 Tokio runtime。
    fn send<'port, 'async_trait>(
        &'port self,
        payload: NotificationPayload,
    ) -> Pin<Box<dyn Future<Output = AppResult<()>> + Send + 'async_trait>>
    where
        'port: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                return Err(AppError::infrastructure("测试通知发送失败"));
            }
            self.payloads
                .lock()
                .expect("lock fake notification payloads")
                .push(payload);
            Ok(())
        })
    }
}

/// 用一次即时 poll 驱动不挂起的 fake adapter，测试不需要真实系统通知或 runtime。
///
/// @author kongweiguang
fn poll_immediate<F: Future>(future: F) -> F::Output {
    let mut context = TaskContext::from_waker(Waker::noop());
    let mut future = Box::pin(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("fake notification future must complete immediately"),
    }
}

/// 构造只启用后台通知的真实策略 service，验证窗口可见性仍由 service 持有。
fn notification_service(
    port: Arc<FakeNotificationPort>,
    enabled: bool,
) -> DesktopNotificationService {
    let service = DesktopNotificationService::new(
        port,
        DesktopNotificationSettings {
            enabled,
            background_only: true,
            ..DesktopNotificationSettings::default()
        },
    );
    service
        .set_window_visibility(WindowVisibility::Hidden)
        .expect("set hidden notification visibility");
    service
}

/// 创建不包含 URL 或签名的 UI 候选，用于通知 payload 与状态绑定验证。
fn update_candidate(version: &str) -> UpdateCandidate {
    UpdateCandidate {
        id: UpdateCandidateId::new(),
        version: version.to_owned(),
        notes: None,
        published_at: None,
    }
}

/// 恢复项 fixture 只含 UI 可见元数据，不引用真实配置路径或安装包。
fn recovery_record(id: UpdateRecoveryId) -> UpdateRecoveryRecord {
    UpdateRecoveryRecord {
        id,
        version: "0.4.2".to_owned(),
        target: "windows-x86_64".to_owned(),
        artifact_sha256: "a".repeat(64),
        state: UpdateRecoveryState::OutcomeUnknown,
        recovery_path: std::path::PathBuf::from("isolated/recovery"),
    }
}

/// 构造不触发安装动作的已下载句柄，用来验证宿主回执身份绑定。
fn downloaded_update(id: DownloadedUpdateId) -> DownloadedUpdate {
    DownloadedUpdate {
        id,
        version: "0.4.2".to_owned(),
    }
}

#[test]
/// 进度超过服务端声明总量时仍展示安全的 100% 上限和可读单位。
fn known_total_formats_bytes_and_clamps_percentage() {
    let text = format_progress(UpdateProgress {
        received_bytes: 2 * 1024,
        total_bytes: Some(1024),
    });

    assert_eq!(text, "100% · 2.0 KiB / 1.0 KiB");
}

#[test]
/// 未知或零总量不能除零，也不能伪造百分比。
fn unknown_or_zero_total_uses_received_bytes_only() {
    assert_eq!(
        format_progress(UpdateProgress {
            received_bytes: 512,
            total_bytes: None,
        }),
        format!("{} 512 B", localized("已接收", "Received"))
    );
    assert_eq!(
        format_progress(UpdateProgress {
            received_bytes: 512,
            total_bytes: Some(0),
        }),
        format!("{} 512 B", localized("已接收", "Received"))
    );
}

#[test]
/// 恢复清理必须绑定确认中的同一条记录，重复点击和陈旧身份不能发起删除。
fn recovery_acknowledgement_requires_matching_confirmation_and_record() {
    let selected_id = UpdateRecoveryId::new();
    let other_id = UpdateRecoveryId::new();
    let records = vec![recovery_record(selected_id)];

    assert!(!can_acknowledge_recovery(
        None,
        None,
        &records,
        selected_id,
        false
    ));
    assert!(can_acknowledge_recovery(
        Some(selected_id),
        None,
        &records,
        selected_id,
        false
    ));
    assert!(!can_acknowledge_recovery(
        Some(selected_id),
        Some(selected_id),
        &records,
        selected_id,
        false
    ));
    assert!(!can_acknowledge_recovery(
        Some(other_id),
        None,
        &records,
        selected_id,
        false
    ));
    assert!(!can_acknowledge_recovery(
        Some(selected_id),
        None,
        &records,
        selected_id,
        true
    ));
}

#[test]
/// Unknown 恢复状态使用明确人工核实文案，避免被误解成可自动重装。
fn unknown_recovery_label_is_manual_review_only() {
    assert_eq!(
        recovery_state_label(UpdateRecoveryState::OutcomeUnknown),
        localized("安装结果未知", "Installation result unknown")
    );
}

#[test]
/// About 元数据必须来自当前 Cargo package，许可证正文必须来自仓库根 LICENSE。
fn about_metadata_uses_package_values_and_embedded_root_license() {
    assert_eq!(APP_VERSION, env!("CARGO_PKG_VERSION"));
    assert_eq!(LICENSE_SPDX, env!("CARGO_PKG_LICENSE"));
    assert_eq!(PROJECT_REPOSITORY, env!("CARGO_PKG_REPOSITORY"));
    assert!(repository_url().is_some());
    assert!(
        LICENSE_TEXT
            .trim_start()
            .starts_with("GNU GENERAL PUBLIC LICENSE")
    );
}

#[test]
/// 重启失败只能覆盖对应的 RestartPending 项，不能改写新更新或陈旧状态。
fn restart_failure_requires_the_matching_pending_update() {
    let update_id = DownloadedUpdateId::new();
    let other_id = DownloadedUpdateId::new();
    let pending = UpdatePanelState::RestartPending(downloaded_update(update_id));
    let ready = UpdatePanelState::ReadyToInstall(downloaded_update(update_id));

    assert!(restart_failure_matches(&pending, update_id));
    assert!(!restart_failure_matches(&pending, other_id));
    assert!(!restart_failure_matches(&ready, update_id));
}

#[test]
/// listener 恢复失败只绑定正在安装的句柄，并强制走退出人工恢复而不是重试。
fn listener_recovery_failure_disables_install_retry() {
    let update_id = DownloadedUpdateId::new();
    let other_id = DownloadedUpdateId::new();
    let installing = UpdatePanelState::Installing(downloaded_update(update_id));
    let host_failed = UpdatePanelState::HostRecoveryFailed {
        update: downloaded_update(update_id),
        original_result: UpdateInstallResult::NotStarted(AppError::unsupported("test")),
        safe_message: "listener restore failed".to_owned(),
    };
    let safely_retryable = UpdatePanelState::StoppedUpdateFailed {
        update: downloaded_update(update_id),
        can_retry: true,
    };

    assert!(listener_recovery_failure_matches(&installing, update_id));
    assert!(!listener_recovery_failure_matches(&installing, other_id));
    assert!(!listener_recovery_failure_matches(
        &UpdatePanelState::RestartPending(downloaded_update(update_id)),
        update_id
    ));
    assert!(!can_retry_host_install(&host_failed));
    assert!(can_exit_after_install_failure(&host_failed));
    assert!(can_retry_host_install(&safely_retryable));
}

#[test]
/// 仅未取消的 Some(candidate) 可产生通知；None、错误和取消都不触发系统提醒。
fn only_a_current_successful_candidate_is_eligible_for_notification() {
    let candidate = update_candidate("0.4.2");
    let available: AppResult<Option<UpdateCandidate>> = Ok(Some(candidate.clone()));
    let no_update: AppResult<Option<UpdateCandidate>> = Ok(None);
    let failed: AppResult<Option<UpdateCandidate>> = Err(AppError::infrastructure("检查失败"));

    assert_eq!(
        notification_candidate_for_check(&available, false),
        Some(candidate)
    );
    assert!(notification_candidate_for_check(&available, true).is_none());
    assert!(notification_candidate_for_check(&no_update, false).is_none());
    assert!(notification_candidate_for_check(&failed, false).is_none());
}

#[test]
/// 检查回执只能消费匹配的 operation 一次，陈旧或重复回执不会再排通知任务。
fn check_operation_generation_is_matched_and_consumed_once() {
    let state = UpdatePanelState::Checking;
    let mut active_generation = Some(17);

    assert!(!take_check_operation(&mut active_generation, &state, 16));
    assert_eq!(active_generation, Some(17));
    assert!(take_check_operation(&mut active_generation, &state, 17));
    assert_eq!(active_generation, None);
    assert!(!take_check_operation(&mut active_generation, &state, 17));
}

#[test]
/// 真正 UpdateAvailable 通知沿用 service 的可见性和偏好过滤，并传递脱敏版本摘要。
fn sends_update_available_payload_through_the_shared_notification_service() {
    let port = Arc::new(FakeNotificationPort::default());
    let service = notification_service(port.clone(), true);

    let disposition = poll_immediate(notify_update_available(&service, "0.4.2"))
        .expect("submit update notification");

    assert_eq!(disposition, NotificationDisposition::Submitted);
    assert_eq!(port.calls.load(Ordering::Relaxed), 1);
    let payloads = port.payloads.lock().expect("read fake payloads");
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].title, "Kerminal 更新可用");
    assert_eq!(payloads[0].body, "版本 0.4.2 已可用。");
}

#[test]
/// 用户关闭通知时由共享 service 真实过滤，不调用平台 port。
fn disabled_notification_settings_do_not_call_the_platform_port() {
    let port = Arc::new(FakeNotificationPort::default());
    let service = notification_service(port.clone(), false);

    let disposition = poll_immediate(notify_update_available(&service, "0.4.2"))
        .expect("disabled notification is a successful policy result");

    assert_eq!(disposition, NotificationDisposition::Disabled);
    assert_eq!(port.calls.load(Ordering::Relaxed), 0);
}

#[test]
/// OS 通知失败保留安全诊断，但当前 Available 候选及版本状态不被改写。
fn notification_failure_keeps_the_update_candidate_unchanged() {
    let candidate = update_candidate("0.4.2");
    let state = UpdatePanelState::Available(candidate.clone());
    let generation = 23;
    let port = Arc::new(FakeNotificationPort {
        fail: true,
        ..FakeNotificationPort::default()
    });
    let service = notification_service(port.clone(), true);

    let result = poll_immediate(notify_update_available(&service, &candidate.version));
    let diagnostic = update_notification_diagnostic(
        Some((generation, candidate.id)),
        generation,
        candidate.id,
        &result,
    );
    let stale_diagnostic = update_notification_diagnostic(
        Some((generation, candidate.id)),
        generation + 1,
        candidate.id,
        &result,
    );

    assert_eq!(port.calls.load(Ordering::Relaxed), 1);
    assert_eq!(diagnostic.as_deref(), Some("测试通知发送失败"));
    assert!(stale_diagnostic.is_none());
    match state {
        UpdatePanelState::Available(current) => assert_eq!(current, candidate),
        _ => panic!("notification failure must preserve the available candidate"),
    }
}

#[test]
/// About 只将可用候选下载后安装，已验签包直接安装，其余状态不重复启动事务。
fn about_install_action_selects_one_safe_update_step() {
    let available = UpdatePanelState::Available(update_candidate("0.4.2"));
    let verified = UpdatePanelState::ReadyToInstall(downloaded_update(DownloadedUpdateId::new()));
    let downloading = UpdatePanelState::Downloading(update_candidate("0.4.2"));

    assert_eq!(
        settings_install_step(&available),
        SettingsInstallStep::DownloadThenInstall
    );
    assert_eq!(
        settings_install_step(&verified),
        SettingsInstallStep::InstallVerified
    );
    assert_eq!(
        settings_install_step(&downloading),
        SettingsInstallStep::Ignore
    );
}

#[test]
/// 单步安装只在 About 已请求且下载返回已验签句柄后自动进入 Root 安装保护。
fn about_install_consumes_request_and_continues_only_after_verified_download_success() {
    let verified = Ok(downloaded_update(DownloadedUpdateId::new()));
    let failed: AppResult<DownloadedUpdate> = Err(AppError::infrastructure("下载失败"));
    let cancelled: AppResult<DownloadedUpdate> = Err(AppError::cancelled("下载取消"));
    let mut requested = true;
    let mut failed_request = true;
    let mut cancelled_request = true;

    assert!(take_settings_auto_install_after_download(
        &mut requested,
        false,
        &verified
    ));
    assert!(!requested);
    assert!(!take_settings_auto_install_after_download(
        &mut failed_request,
        false,
        &failed
    ));
    assert!(!failed_request);
    assert!(!take_settings_auto_install_after_download(
        &mut cancelled_request,
        true,
        &verified
    ));
    assert!(!cancelled_request);
    assert!(!take_settings_auto_install_after_download(
        &mut cancelled_request,
        true,
        &cancelled
    ));
}

#[test]
/// 手动检查不能取代正在安装的已验签包，也不能覆盖忙碌和不可用状态。
fn about_update_check_preserves_staged_packages_and_terminal_states() {
    let available = UpdatePanelState::Available(update_candidate("0.4.2"));
    let verified = UpdatePanelState::ReadyToInstall(downloaded_update(DownloadedUpdateId::new()));
    let restarting = UpdatePanelState::RestartPending(downloaded_update(DownloadedUpdateId::new()));

    assert!(settings_check_allowed(&available, true, false));
    assert!(!settings_check_allowed(&available, false, false));
    assert!(!settings_check_allowed(&available, true, true));
    assert!(!settings_check_allowed(&verified, true, false));
    assert!(!settings_check_allowed(&restarting, true, false));
    assert!(can_check_for_updates(&available));
    assert!(!can_check_for_updates(&verified));
    assert!(!can_check_for_updates(&restarting));
}

#[test]
/// 已配置更新源的检查失败保持可重试语义，不误报为安装形态不可用。
fn idle_update_status_distinguishes_failure_from_unavailable_service() {
    assert_eq!(
        idle_update_status_label(true, true),
        localized("失败", "Failed")
    );
    assert_eq!(
        idle_update_status_label(false, true),
        localized("不可用", "Unavailable")
    );
    assert_eq!(
        idle_update_status_label(true, false),
        localized("可检查", "Ready to check")
    );
}
