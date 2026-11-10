//! 扩展重启确认使用真实会话身份，不把关闭后的残留会话计作活动连接。
//! @author kongweiguang

use super::active_session_ids;
use kerminal_domain::{
    identity::{HostId, SessionId},
    sessions::{
        LocalSessionTarget, SessionInfo, SessionLifecycle, SessionOwner, SessionTarget,
        SshSessionTarget, TerminalSize,
    },
};
use std::{collections::BTreeMap, ffi::OsString};

/// 样本复用领域会话结构，测试确认统计不依赖可见标签。
fn sample(id: &str, target: SessionTarget, lifecycle: SessionLifecycle) -> SessionInfo {
    SessionInfo {
        session_id: SessionId::parse(id).unwrap(),
        target,
        owner: SessionOwner::Mcp,
        size: TerminalSize::default(),
        lifecycle,
        exit_status: None,
    }
}

/// 结果以身份排序并区分 SSH；状态变化可触发确认后重新核对。
#[test]
fn counts_all_live_owners_and_excludes_closed_sessions() {
    let local = || {
        SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("shell"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        })
    };
    let ssh = || {
        SessionTarget::Ssh(SshSessionTarget {
            host_id: HostId::new(),
            working_directory: None,
            command: None,
        })
    };
    let sessions = vec![
        sample("z-ssh", ssh(), SessionLifecycle::Running),
        sample("a-local", local(), SessionLifecycle::Starting),
        sample("closed", local(), SessionLifecycle::Closed),
    ];
    assert_eq!(
        active_session_ids(&sessions),
        vec![("a-local".into(), false), ("z-ssh".into(), true)]
    );
    assert_ne!(
        active_session_ids(&sessions),
        active_session_ids(&sessions[..1])
    );
}

/// 路由数量相同时代次变化仍要求重新确认，防止新连接继承旧授权。
#[test]
fn changed_ssh_route_generation_invalidates_confirmation() {
    use super::RestartActivity;
    use kerminal_application::connections::RemoteConnectionActivity;
    let first = RestartActivity {
        sessions: Vec::new(),
        remote: Some(RemoteConnectionActivity {
            routes: 1,
            generation: 1,
        }),
    };
    let mut replacement = first.clone();
    replacement.remote.as_mut().unwrap().generation += 1;
    assert_ne!(first, replacement);
}
