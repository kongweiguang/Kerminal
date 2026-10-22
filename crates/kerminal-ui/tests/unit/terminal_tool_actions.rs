//! 根窗口终端日志元数据投影测试。
//!
//! @author kongweiguang

use super::{broadcast_binding_is_current, session_log_metadata, ui_binding_matches_context};
use kerminal_application::sessions::{
    TerminalCommandAvailability, TerminalCommandContext, TerminalCommandUnavailableReason,
    UiSessionBinding,
};
use kerminal_domain::connections::RemoteShellDialect;
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::{
        ContainerRuntime, ContainerSessionTarget, LocalSessionTarget, SerialSessionTarget,
        SessionInfo, SessionLifecycle, SessionOwner, SessionTarget, SshSessionTarget,
        TelnetSessionTarget, TerminalSize,
    },
};
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

/// 日志头只使用本地目标的 shell 和初始工作目录，不将运行输出解析为可信元数据。
#[test]
fn local_log_metadata_comes_from_the_session_target() {
    let session = session(SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("pwsh"),
        args: Vec::new(),
        working_directory: Some(PathBuf::from("work")),
        environment: BTreeMap::new(),
    }));

    assert_eq!(
        session_log_metadata(&session),
        ("pwsh".to_owned(), Some("work".to_owned()))
    );
}

/// 远端日志使用协议标签和用户请求的工作目录，不尝试访问远端 shell 环境猜测路径。
#[test]
fn ssh_log_metadata_preserves_explicit_remote_working_directory() {
    let session = session(SessionTarget::Ssh(SshSessionTarget {
        host_id: kerminal_domain::identity::HostId::parse("host-a").expect("valid fixture host id"),
        working_directory: Some(PathBuf::from("remote-work")),
        command: None,
    }));

    assert_eq!(
        session_log_metadata(&session),
        ("ssh".to_owned(), Some("remote-work".to_owned()))
    );
}

/// 不存在可确认工作目录的协议使用稳定标签且不编造 cwd。
#[test]
fn non_shell_targets_do_not_invent_a_working_directory() {
    let telnet = session(SessionTarget::Telnet(TelnetSessionTarget {
        host: "example.test".to_owned(),
        port: 23,
    }));
    let serial = session(SessionTarget::Serial(SerialSessionTarget::new(
        PathBuf::from("COM1"),
    )));
    let container = session(SessionTarget::Container(ContainerSessionTarget {
        host_id: None,
        runtime: ContainerRuntime::Docker,
        container_id: "container-a".to_owned(),
        command: vec![OsString::from("sh")],
    }));

    assert_eq!(session_log_metadata(&telnet), ("telnet".to_owned(), None));
    assert_eq!(session_log_metadata(&serial), ("serial".to_owned(), None));
    assert_eq!(
        session_log_metadata(&container),
        ("container".to_owned(), None)
    );
}

/// 广播启动和每次输入都依赖 UI 注册的完整绑定，重连代次变化后不能沿用旧目标。
#[test]
fn broadcast_binding_rejects_missing_or_reconnected_registration() {
    let binding = ui_binding();
    assert!(broadcast_binding_is_current(&binding, Some(&binding)));

    let mut reconnected = binding.clone();
    reconnected.session_generation += 1;
    assert!(!broadcast_binding_is_current(&binding, Some(&reconnected)));
    assert!(!broadcast_binding_is_current(&binding, None));
}

/// 启动校验必须回读服务端 owner 和 generation，不能用相同 SessionId 掩盖重连或改属。
#[test]
fn broadcast_context_must_match_the_captured_ui_binding() {
    let binding = ui_binding();
    assert!(ui_binding_matches_context(
        &binding,
        &terminal_context(&binding)
    ));

    let mut reconnected = terminal_context(&binding);
    reconnected.session_generation += 1;
    assert!(!ui_binding_matches_context(&binding, &reconnected));

    let mut reassigned = terminal_context(&binding);
    reassigned.owner = SessionOwner::Mcp;
    assert!(!ui_binding_matches_context(&binding, &reassigned));

    let mut other_session = terminal_context(&binding);
    other_session.session_id = SessionId::parse("session-b").expect("valid fixture session id");
    assert!(!ui_binding_matches_context(&binding, &other_session));

    let mut agent_binding = binding.clone();
    agent_binding.owner = SessionOwner::Mcp;
    assert!(!ui_binding_matches_context(
        &agent_binding,
        &terminal_context(&agent_binding)
    ));
}

/// 用新标识创建一个 UI pane binding，测试不依赖固定 UUID 或真实运行时状态。
fn ui_binding() -> UiSessionBinding {
    UiSessionBinding {
        session_id: SessionId::parse("session-a").expect("valid fixture session id"),
        owner: SessionOwner::Ui {
            tab_id: TabId::parse("tab-a").expect("valid fixture tab id"),
            pane_id: PaneId::parse("pane-a").expect("valid fixture pane id"),
        },
        session_generation: 4,
    }
}

/// 只构造广播绑定校验需要的可信 context 字段，不声称 prompt 文本可由 UI 推断。
fn terminal_context(binding: &UiSessionBinding) -> TerminalCommandContext {
    TerminalCommandContext {
        session_id: binding.session_id.clone(),
        owner: binding.owner.clone(),
        session_generation: binding.session_generation,
        prompt_epoch: 3,
        input_generation: 7,
        shell_dialect: RemoteShellDialect::Unknown,
        prompt_ready: false,
        prompt_line_empty: false,
        alternate_screen: false,
        availability: TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::UnsupportedShellDialect,
        ),
    }
}

/// 构造只包含日志投影所需领域字段的运行会话样本。
fn session(target: SessionTarget) -> SessionInfo {
    SessionInfo {
        session_id: SessionId::parse("session-a").expect("valid fixture session id"),
        target,
        owner: SessionOwner::Mcp,
        size: TerminalSize::default(),
        lifecycle: SessionLifecycle::Running,
        exit_status: None,
    }
}
