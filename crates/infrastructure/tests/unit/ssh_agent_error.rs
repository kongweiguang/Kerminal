//! SSH agent 不可用时的公开错误契约。
//!
//! @author kongweiguang

use kerminal_application::PublicErrorCode;

/// 缺失 agent 使用认证错误码，让外部启动能提供显式密码重试，同时不泄露底层路径。
///
/// @author kongweiguang
#[test]
fn unavailable_agent_is_an_authentication_error_with_redacted_source() {
    let error = super::unavailable_ssh_agent(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "private-agent-path",
    ));
    assert_eq!(error.public_code(), PublicErrorCode::AuthenticationFailed);
    assert!(!error.public_message().contains("private-agent-path"));
    assert!(!format!("{error:?}").contains("private-agent-path"));
}
