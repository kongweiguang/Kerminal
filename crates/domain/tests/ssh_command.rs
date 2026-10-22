//! SSH PTY 初始命令的领域契约测试。
//!
//! @author kongweiguang

use kerminal_domain::sessions::SshInitialCommand;

/// Shell command Debug 不应回显可能含凭据或其他私密输入的命令正文。
///
/// @author kongweiguang
#[test]
fn shell_debug_redacts_command_text() {
    let command = SshInitialCommand::Shell("deploy --token secret-value".to_owned());

    assert_eq!(format!("{command:?}"), "Shell(\"[redacted]\")");
}

/// Process Debug 只暴露参数数量，保持程序和每个参数的数据边界但不泄漏其内容。
///
/// @author kongweiguang
#[test]
fn process_debug_redacts_program_and_arguments() {
    let command = SshInitialCommand::Process {
        program: "deploy-secret-path".to_owned(),
        args: vec!["--token".to_owned(), "secret-value".to_owned()],
    };

    let debug = format!("{command:?}");
    assert!(debug.contains("argument_count: 2"));
    assert!(!debug.contains("deploy-secret-path"));
    assert!(!debug.contains("secret-value"));
}
