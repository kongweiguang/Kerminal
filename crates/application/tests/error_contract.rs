//! 应用错误码与脱敏边界契约。
//!
//! @author kongweiguang

use std::{error::Error, io};

use kerminal_application::{AppError, PublicErrorCode};

/// 底层诊断 cause 保留供内部审计，但 Display/Debug 不回传原始错误文本。
#[test]
fn infrastructure_error_retains_source_without_displaying_it() {
    let error = AppError::infrastructure_with_source(
        "本地配置读取失败",
        io::Error::other("secret=/private/path/token"),
    );

    assert_eq!(error.public_code(), PublicErrorCode::Infrastructure);
    assert_eq!(error.to_string(), "本地配置读取失败");
    assert!(!format!("{error:?}").contains("/private/path/token"));
    assert!(error.source().is_some());
}

/// SSH 信任错误必须有独立稳定码，供 UI 触发确认或安全阻断流程。
#[test]
fn host_key_failures_have_distinct_public_codes() {
    assert_eq!(
        AppError::host_key_unknown().public_code().as_str(),
        "host_key_unknown"
    );
    assert_eq!(
        AppError::host_key_changed().public_code().as_str(),
        "host_key_changed"
    );
    assert_eq!(
        AppError::host_key_trust_mismatch().public_code().as_str(),
        "host_key_trust_mismatch"
    );
}

/// 配置损坏错误不冒充宿主当前恢复状态，状态需由 recovery result 单独表达。
#[test]
fn corrupt_configuration_message_does_not_claim_recovery_state() {
    let error = AppError::configuration_corrupt("settings");

    assert_eq!(error.public_code(), PublicErrorCode::ConfigurationCorrupt);
    assert_eq!(error.public_message(), "配置文件损坏，需要恢复");
}

/// 未知提交结果必须有独立稳定码，避免调用方将其误当成可安全回滚的确定失败。
#[test]
fn unknown_commit_result_has_stable_code_and_retains_source() {
    let error = AppError::outcome_unknown_with_source(
        "配置提交结果未知，请重新读取确认",
        io::Error::other("private staging detail"),
    );

    assert_eq!(error.public_code(), PublicErrorCode::OutcomeUnknown);
    assert_eq!(error.public_code().as_str(), "outcome_unknown");
    assert_eq!(error.to_string(), "配置提交结果未知，请重新读取确认");
    assert!(error.source().is_some());
    assert!(!format!("{error:?}").contains("private staging detail"));
}

/// 专用远端断线分类保留 source，但外部错误码维持兼容且不泄露底层连接文案。
///
/// @author kongweiguang
#[test]
fn retryable_remote_disconnect_is_typed_and_redacted() {
    let error = AppError::retryable_remote_disconnect(io::Error::other("private socket detail"));

    assert!(error.is_retryable_remote_disconnect());
    assert_eq!(error.public_code(), PublicErrorCode::Infrastructure);
    assert_eq!(error.to_string(), "远端终端连接意外中断，可重试");
    assert!(!format!("{error:?}").contains("private socket detail"));
    assert!(
        error
            .source()
            .and_then(|source| source.downcast_ref::<io::Error>())
            .is_some()
    );
}

/// 通用错误、显式取消和包装后的诊断错误都不能冒充专用重连信号。
///
/// @author kongweiguang
#[test]
fn unrelated_or_wrapped_errors_are_not_retryable_disconnects() {
    let errors = [
        AppError::infrastructure("普通传输错误"),
        AppError::cancelled("用户取消"),
        AppError::timeout("连接超时"),
        AppError::authentication_failed(),
        AppError::host_key_changed(),
        AppError::remote_exit(Some(7)),
        AppError::retryable_remote_disconnect(io::Error::other("source"))
            .with_source(io::Error::other("wrapped")),
    ];

    assert!(
        errors
            .iter()
            .all(|error| !error.is_retryable_remote_disconnect())
    );
}
