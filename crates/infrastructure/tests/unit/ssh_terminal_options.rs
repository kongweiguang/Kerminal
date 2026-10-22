//! 主机默认值只补充缺省项，终端目标和已明确的恢复位置保持不变。
//!
//! @author kongweiguang

use super::ssh_terminal_open_request;
use kerminal_application::connections::management::HostTerminalOptions;
use kerminal_domain::{
    identity::HostId,
    sessions::{SshSessionTarget, TerminalSize},
};

/// 真实请求投影同时携带 TERM 和 cwd，显式恢复位置始终覆盖主机初始位置。
///
/// @author kongweiguang
#[test]
fn terminal_options_apply_only_when_target_has_no_explicit_directory() {
    let mut target = SshSessionTarget {
        host_id: HostId::new(),
        working_directory: None,
        command: None,
    };
    let options = HostTerminalOptions {
        term: Some("screen-256color".to_owned()),
        initial_working_directory: Some("/srv/default".to_owned()),
    };
    let request = ssh_terminal_open_request(&target, TerminalSize::default(), options.clone());
    assert_eq!(request.term, "screen-256color");
    assert_eq!(request.working_directory.as_deref(), Some("/srv/default"));
    target.working_directory = Some("/srv/restored".into());
    let request = ssh_terminal_open_request(&target, TerminalSize::default(), options);
    assert_eq!(request.working_directory.as_deref(), Some("/srv/restored"));
    let default = ssh_terminal_open_request(
        &SshSessionTarget {
            working_directory: None,
            ..target
        },
        TerminalSize::default(),
        Default::default(),
    );
    assert_eq!(default.term, "xterm-256color");
    assert!(default.working_directory.is_none());
}
