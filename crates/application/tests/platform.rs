//! 桌面平台策略的独立契约测试，不需要创建原生窗口或访问操作系统设置。  
//!
//! @author kongweiguang

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, PublicErrorCode,
    platform::{
        DesktopNotificationEvent, DesktopNotificationService, ExternalActivation,
        ExternalProtocolPort, ExternalProtocolService, ExternalProtocolStatus,
        NotificationDisposition, NotificationPayload, NotificationPort, WindowVisibility,
    },
    preferences::settings::DesktopNotificationSettings as PreferenceNotificationSettings,
};

#[derive(Default)]
struct FakeNotificationPort {
    sent: Mutex<Vec<NotificationPayload>>,
    fail_once: Mutex<bool>,
}

#[async_trait]
impl NotificationPort for FakeNotificationPort {
    /// 测试 adapter 捕获最终文案以校验策略与脱敏，不依赖 Windows toast 或 Linux D-Bus。
    async fn send(&self, payload: NotificationPayload) -> kerminal_application::AppResult<()> {
        let mut fail_once = self.fail_once.lock().expect("fake notification lock");
        if *fail_once {
            *fail_once = false;
            return Err(AppError::infrastructure("模拟通知服务故障"));
        }
        drop(fail_once);
        self.sent
            .lock()
            .expect("fake notification lock")
            .push(payload);
        Ok(())
    }
}

struct FakeExternalProtocolPort {
    status: Mutex<ExternalProtocolStatus>,
    registration_conflict: Mutex<bool>,
    registration_outcome_unknown: Mutex<bool>,
    panic_on_status: Mutex<bool>,
    panic_after_registration: Mutex<bool>,
}

impl FakeExternalProtocolPort {
    /// 隔离测试中的协议关联状态，不接触真实用户注册表或系统设置。
    ///
    /// @author kongweiguang
    fn new() -> Self {
        Self {
            status: Mutex::new(ExternalProtocolStatus {
                supported: true,
                registered: false,
            }),
            registration_conflict: Mutex::new(false),
            registration_outcome_unknown: Mutex::new(false),
            panic_on_status: Mutex::new(false),
            panic_after_registration: Mutex::new(false),
        }
    }
}

impl ExternalProtocolPort for FakeExternalProtocolPort {
    /// 返回 fake 当前状态并支持注入 worker panic，用于验证 JoinError 的 typed 映射。
    ///
    /// @author kongweiguang
    fn status(&self) -> kerminal_application::AppResult<ExternalProtocolStatus> {
        if *self.panic_on_status.lock().expect("fake status panic lock") {
            panic!("simulated external protocol status worker panic");
        }
        Ok(*self.status.lock().expect("fake protocol status lock"))
    }

    /// 模拟显式注册、冲突和写入后 worker panic，确保未知结果可由独立状态读回确认。
    ///
    /// @author kongweiguang
    fn register_for_current_user(&self) -> kerminal_application::AppResult<ExternalProtocolStatus> {
        if *self
            .registration_conflict
            .lock()
            .expect("fake protocol conflict lock")
        {
            return Err(AppError::conflict("kerminal:// 被其他程序占用"));
        }
        let mut status = self.status.lock().expect("fake protocol status lock");
        status.registered = true;
        if *self
            .registration_outcome_unknown
            .lock()
            .expect("fake protocol unknown lock")
        {
            return Err(AppError::outcome_unknown("模拟注册后回执丢失"));
        }
        let receipt = *status;
        drop(status);
        if *self
            .panic_after_registration
            .lock()
            .expect("fake protocol panic lock")
        {
            panic!("simulated external protocol worker panic after write");
        }
        Ok(receipt)
    }

    /// 注销只更新 fake 状态，确保测试不会调用平台注册表适配器。
    ///
    /// @author kongweiguang
    fn unregister_for_current_user(
        &self,
    ) -> kerminal_application::AppResult<ExternalProtocolStatus> {
        let mut status = self.status.lock().expect("fake protocol status lock");
        status.registered = false;
        Ok(*status)
    }
}

/// 单实例 IPC 负载接受合理参数并拒绝超长或 NUL 注入数据。
#[test]
fn external_activation_is_bounded_and_redacted_in_debug() {
    let activation = ExternalActivation {
        arguments: vec!["kerminal://ssh?token=secret-value".to_owned()],
        working_directory: Some("C:\\Users\\Alice\\Work".to_owned()),
    };
    activation.validate().expect("valid launch request");
    let debug = format!("{activation:?}");
    assert!(!debug.contains("secret-value"));
    assert!(!debug.contains("Alice"));

    let too_many_arguments = ExternalActivation {
        arguments: vec!["arg".to_owned(); 129],
        working_directory: None,
    };
    assert!(too_many_arguments.validate().is_err());
    let nul_argument = ExternalActivation {
        arguments: vec!["unsafe\0argument".to_owned()],
        working_directory: None,
    };
    assert!(nul_argument.validate().is_err());
}

/// typed service 的注册与注销回执来自 fake 状态回读，不访问操作系统协议设置。
///
/// @author kongweiguang
#[tokio::test]
async fn external_protocol_service_returns_read_back_state() {
    let port = Arc::new(FakeExternalProtocolPort::new());
    let service = ExternalProtocolService::new(port.clone());

    assert!(
        !service
            .status()
            .await
            .expect("initial fake status")
            .registered
    );
    assert!(
        service
            .enable_for_current_user()
            .await
            .expect("fake registration")
            .registered
    );
    assert!(
        !service
            .disable_for_current_user()
            .await
            .expect("fake unregistration")
            .registered
    );
}

/// fake 外部 handler 的冲突必须保留原状态并作为明确 typed error 返回。
///
/// @author kongweiguang
#[tokio::test]
async fn external_protocol_service_preserves_conflicting_handler() {
    let port = Arc::new(FakeExternalProtocolPort::new());
    *port
        .registration_conflict
        .lock()
        .expect("fake protocol conflict lock") = true;
    let service = ExternalProtocolService::new(port.clone());

    let error = service
        .enable_for_current_user()
        .await
        .expect_err("a different handler must not be replaced");
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert!(
        !service
            .status()
            .await
            .expect("fake status remains readable")
            .registered
    );
}

/// 未知回执不伪装成成功；后续状态读取只表示当前状态，不改写原回执类别。
///
/// @author kongweiguang
#[tokio::test]
async fn external_protocol_service_preserves_unknown_operation_result() {
    let port = Arc::new(FakeExternalProtocolPort::new());
    *port
        .registration_outcome_unknown
        .lock()
        .expect("fake protocol unknown lock") = true;
    let service = ExternalProtocolService::new(port);

    let error = service
        .enable_for_current_user()
        .await
        .expect_err("a lost receipt stays unknown");
    assert_eq!(error.public_code(), PublicErrorCode::OutcomeUnknown);
    assert!(
        service
            .status()
            .await
            .expect("read current fake state")
            .registered
    );
}

/// 状态 worker 崩溃返回基础设施错误，注册 worker 写入后崩溃则必须返回未知结果并支持回读。
///
/// @author kongweiguang
#[tokio::test]
async fn external_protocol_service_classifies_blocking_worker_panics() {
    let status_port = Arc::new(FakeExternalProtocolPort::new());
    *status_port
        .panic_on_status
        .lock()
        .expect("fake status panic lock") = true;
    let status_error = ExternalProtocolService::new(status_port)
        .status()
        .await
        .expect_err("a panicking status worker must not appear successful");
    assert_eq!(status_error.public_code(), PublicErrorCode::Infrastructure);

    let write_port = Arc::new(FakeExternalProtocolPort::new());
    *write_port
        .panic_after_registration
        .lock()
        .expect("fake protocol panic lock") = true;
    let write_service = ExternalProtocolService::new(write_port);
    let write_error = write_service
        .enable_for_current_user()
        .await
        .expect_err("a panicking write worker has an unknown result");
    assert_eq!(write_error.public_code(), PublicErrorCode::OutcomeUnknown);
    assert!(
        write_service
            .status()
            .await
            .expect("state remains readable after worker panic")
            .registered
    );
}

/// 通知默认关闭，开启后应用窗口前台可按设置过滤短任务和低重要性事件。
/// @author kongweiguang
#[tokio::test]
async fn desktop_notification_policy_respects_settings_and_visibility() {
    let adapter = Arc::new(FakeNotificationPort::default());
    let service =
        DesktopNotificationService::new(adapter.clone(), PreferenceNotificationSettings::default());
    let event = DesktopNotificationEvent::SftpBatchCompleted {
        host_label: Some("prod".to_owned()),
        total_count: 2,
        succeeded_count: Some(2),
        failed_count: Some(0),
        duration: Duration::from_secs(2),
        importance: None,
    };

    assert_eq!(
        service
            .notify(event.clone(), WindowVisibility::Hidden)
            .await
            .expect("disabled policy is a normal skip"),
        NotificationDisposition::Disabled
    );

    let foreground_settings = PreferenceNotificationSettings {
        enabled: true,
        background_only: false,
        ..PreferenceNotificationSettings::default()
    };
    service
        .update_settings(foreground_settings)
        .expect("apply current preference snapshot");
    assert_eq!(
        service
            .notify(event.clone(), WindowVisibility::Foreground)
            .await
            .expect("short event is a normal skip"),
        NotificationDisposition::ForegroundShortEvent
    );
    assert!(
        adapter
            .sent
            .lock()
            .expect("fake notification lock")
            .is_empty()
    );

    assert_eq!(
        service
            .notify(event, WindowVisibility::Hidden)
            .await
            .expect("hidden window receives the completion notice"),
        NotificationDisposition::Submitted
    );
}

/// 高优先级错误可绕过前台耗时抑制；相同事件发送成功后按用户节流值去重。
#[tokio::test]
async fn desktop_notification_policy_keeps_urgent_failures_and_throttles() {
    let adapter = Arc::new(FakeNotificationPort::default());
    let settings = PreferenceNotificationSettings {
        enabled: true,
        background_only: false,
        important_only: true,
        min_duration_ms: 120_000,
        throttle_ms: 600_000,
    };
    let service = DesktopNotificationService::new(adapter.clone(), settings);
    let event = DesktopNotificationEvent::SftpTransferFailed {
        host_label: Some("prod".to_owned()),
        failed_count: 1,
        importance: None,
    };
    assert_eq!(
        service
            .notify(event.clone(), WindowVisibility::Foreground)
            .await
            .expect("high priority failure is submitted"),
        NotificationDisposition::Submitted
    );
    assert_eq!(
        service
            .notify(event, WindowVisibility::Hidden)
            .await
            .expect("duplicate notification is throttled"),
        NotificationDisposition::Throttled
    );
    assert_eq!(
        adapter.sent.lock().expect("fake notification lock").len(),
        1
    );
}

/// 通知失败后释放节流占位，并在文案中隐藏令牌、口令和本机用户目录。
#[tokio::test]
async fn notification_failure_can_retry_and_payload_is_sanitized() {
    let adapter = Arc::new(FakeNotificationPort {
        sent: Mutex::new(Vec::new()),
        fail_once: Mutex::new(true),
    });
    let settings = PreferenceNotificationSettings {
        enabled: true,
        background_only: false,
        throttle_ms: 600_000,
        ..PreferenceNotificationSettings::default()
    };
    let service = DesktopNotificationService::new(adapter.clone(), settings);
    let event = DesktopNotificationEvent::McpServerFailed {
        port: Some(9000),
        reason: Some(
            "Bearer live-token-value token=sk-abcdefghijklmnop password=hunter2 C:\\Users\\Alice\\.ssh\\id_ed25519 -----BEGIN PRIVATE KEY-----private-material-----END PRIVATE KEY-----".to_owned(),
        ),
        importance: None,
    };
    assert!(
        service
            .notify(event.clone(), WindowVisibility::Hidden)
            .await
            .is_err()
    );
    assert_eq!(
        service
            .notify(event, WindowVisibility::Hidden)
            .await
            .expect("failed send leaves retry available"),
        NotificationDisposition::Submitted
    );
    let sent = adapter.sent.lock().expect("fake notification lock");
    assert_eq!(sent.len(), 1);
    assert!(!sent[0].body.contains("hunter2"));
    assert!(!sent[0].body.contains("live-token-value"));
    assert!(!sent[0].body.contains("private-material"));
    assert!(!sent[0].body.contains("Alice"));
    assert!(sent[0].body.contains("token=[已隐藏令牌]"));
    assert!(sent[0].body.contains("[已隐藏令牌]"));
    assert!(sent[0].body.contains("[已隐藏私钥]"));
    assert!(sent[0].body.len() <= 180);
}

/// 同一通知服务在偏好保存后改用新策略，而无需重新装配平台端口。
#[tokio::test]
async fn desktop_notification_uses_updated_runtime_settings() {
    let adapter = Arc::new(FakeNotificationPort::default());
    let service =
        DesktopNotificationService::new(adapter.clone(), PreferenceNotificationSettings::default());
    let event = DesktopNotificationEvent::SftpTransferFailed {
        host_label: None,
        failed_count: 1,
        importance: None,
    };

    assert_eq!(
        service
            .notify(event.clone(), WindowVisibility::Hidden)
            .await
            .expect("disabled preferences skip the event"),
        NotificationDisposition::Disabled
    );
    service
        .update_settings(PreferenceNotificationSettings {
            enabled: true,
            background_only: false,
            ..PreferenceNotificationSettings::default()
        })
        .expect("update runtime notification policy");
    assert_eq!(
        service
            .notify(event, WindowVisibility::Hidden)
            .await
            .expect("new policy is effective immediately"),
        NotificationDisposition::Submitted
    );
    assert_eq!(
        adapter.sent.lock().expect("fake notification lock").len(),
        1
    );
}
