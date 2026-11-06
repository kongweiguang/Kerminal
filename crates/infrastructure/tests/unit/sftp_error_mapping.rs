//! 协议错误只公开固定原因，不能把远端动态文本带入传输卡片。
//!
//! @author kongweiguang

use kerminal_application::PublicErrorCode;

use super::{SftpError, map_sftp_error};

/// 配额失败应有可判读原因，同时隔离库错误中可能携带的主机、路径和凭据文本。
///
/// @author kongweiguang
#[test]
fn sftp_limit_failure_keeps_a_specific_safe_reason() {
    let error = map_sftp_error(SftpError::Limited("private-host/private-file".into()));

    assert_eq!(error.public_code(), PublicErrorCode::Infrastructure);
    assert_eq!(error.public_message(), "SFTP 资源配额不足或请求超过限制");
    assert!(!error.public_message().contains("private-host"));
    assert!(!error.public_message().contains("private-file"));
}
