//! RDP 启动状态文案约束，重点防止把系统委派误报成客户端已启动或登录。
//!
//! @author kongweiguang

use kerminal_application::{
    PublicErrorCode,
    connections::{RdpClientLifecycle, RdpLaunchStatus},
};

use super::rdp_status_text;

/// 系统 broker 的受理与完成状态都不证明外部 RDP 客户端已启动或远端已登录。
#[test]
fn delegated_status_never_claims_client_launch_or_login() {
    let (requested, requested_is_error) = rdp_status_text(
        RdpLaunchStatus::Requested,
        RdpClientLifecycle::DelegatedToSystem,
    );
    let (delegated, delegated_is_error) = rdp_status_text(
        RdpLaunchStatus::DelegatedToSystem,
        RdpClientLifecycle::DelegatedToSystem,
    );

    assert!(!requested_is_error);
    assert!(!delegated_is_error);
    assert!(
        requested.contains("已请求系统打开 RDP 文件") || requested.contains("was asked to open")
    );
    assert!(
        delegated.contains("系统已处理打开 RDP 文件的请求")
            || delegated.contains("finished handling")
    );
    assert!(!requested.contains("客户端已启动") && !requested.contains("client started"));
    assert!(!delegated.contains("客户端已启动") && !delegated.contains("client started"));
}

/// RDP 失败提示仅使用公开错误码，且以错误语义呈现给当前主机详情。
#[test]
fn failed_status_is_presented_as_an_error_with_public_code() {
    let code = PublicErrorCode::Infrastructure;
    let (message, is_error) = rdp_status_text(
        RdpLaunchStatus::Failed { code },
        RdpClientLifecycle::ManagedProcess,
    );

    assert!(is_error);
    assert!(message.contains(code.as_str()));
}
