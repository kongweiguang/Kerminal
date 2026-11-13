//! 验收 UI/关闭结果合并回归。
//!
//! @author kongweiguang

use std::{error::Error, fmt, io};

use kerminal_application::{AppError, AppResult};

use super::acceptance_outcome;

/// 带来源链的 UI 错误类型，用于证明单侧失败返回的是原错误而不是重新包装的字符串。
///
/// @author kongweiguang
#[derive(Debug)]
struct UiMarkerError {
    message: &'static str,
    source: io::Error,
}

impl fmt::Display for UiMarkerError {
    /// 只输出测试标记，来源链由 `Error::source` 单独检查。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl Error for UiMarkerError {
    /// 保留底层原因，验证合并入口不会在单边失败时丢弃来源。
    ///
    /// @author kongweiguang
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// 构造 UI 侧失败，模拟隐藏窗口流程已经返回错误但组合根仍继续关闭。
///
/// @author kongweiguang
fn ui_failure(message: &'static str) -> Result<(), Box<dyn Error>> {
    Err(Box::new(UiMarkerError {
        message,
        source: io::Error::other("ui-source"),
    }))
}

/// 构造组合根关闭失败，验证它不会被 UI 失败分支吞掉。
///
/// @author kongweiguang
fn shutdown_failure(message: &'static str) -> AppResult<()> {
    Err(AppError::invalid(message))
}

/// 两侧同时失败时必须保留两条原因，避免只看到先返回的 UI 错误而误以为回收已完成。
///
/// @author kongweiguang
#[test]
fn combines_ui_and_shutdown_failures() {
    let message = acceptance_outcome(ui_failure("ui-proof"), shutdown_failure("shutdown-proof"))
        .expect_err("both failures must remain an error")
        .to_string();

    assert!(message.contains("ui-proof"));
    assert!(message.contains("shutdown-proof"));
}

/// 仅 UI 失败时返回原始错误对象，类型与 `source` 链都不能被字符串合并替换。
///
/// @author kongweiguang
#[test]
fn reports_ui_failure_alone_with_original_type_and_source() {
    let error = acceptance_outcome(ui_failure("ui-only"), Ok(()))
        .expect_err("UI failure must remain an error");

    assert!(error.downcast_ref::<UiMarkerError>().is_some());
    assert_eq!(
        error.source().map(|source| source.to_string()).as_deref(),
        Some("ui-source")
    );
    assert!(!error.to_string().contains("组合根关闭失败"));
}

/// UI 成功但组合根关闭失败时仍返回原始 AppError，保留其脱敏文案和来源链。
///
/// @author kongweiguang
#[test]
fn reports_shutdown_failure_alone_with_original_type_and_source() {
    let shutdown =
        AppError::infrastructure_with_source("shutdown-proof", io::Error::other("shutdown-source"));
    let error = acceptance_outcome(Ok(()), Err(shutdown))
        .expect_err("shutdown failure must remain an error");

    assert!(error.downcast_ref::<AppError>().is_some());
    assert_eq!(error.to_string(), "shutdown-proof");
    assert_eq!(
        error.source().map(|source| source.to_string()).as_deref(),
        Some("shutdown-source")
    );
    assert!(!error.to_string().contains("GPUI 右栏验收失败"));
}

/// 两侧都正常时才视为成功。
///
/// @author kongweiguang
#[test]
fn accepts_only_when_both_sides_succeed() {
    assert!(acceptance_outcome(Ok(()), Ok(())).is_ok());
}
