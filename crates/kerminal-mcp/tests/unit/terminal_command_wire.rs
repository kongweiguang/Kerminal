//! SSH 初始命令的 MCP wire 映射回归测试。
//!
//! @author kongweiguang

use kerminal_domain::sessions::SshInitialCommand;
use serde_json::json;

use super::ssh_initial_command_value;

/// Shell 文本按原始脚本保留，wire 不把它误解释为可执行程序与 argv。
#[test]
fn shell_command_keeps_script_semantics() {
    let command = SshInitialCommand::Shell("printf '%s' hello".to_owned());

    assert_eq!(
        ssh_initial_command_value(Some(&command)),
        json!({ "kind": "shell", "script": "printf '%s' hello" })
    );
}

/// 结构化启动命令逐项输出 argv，避免空格拼接丢失参数边界。
#[test]
fn process_command_keeps_program_and_argument_boundaries() {
    let command = SshInitialCommand::Process {
        program: "tool with space".to_owned(),
        args: vec!["--value".to_owned(), "two words".to_owned()],
    };

    assert_eq!(
        ssh_initial_command_value(Some(&command)),
        json!({
            "kind": "process",
            "program": "tool with space",
            "args": ["--value", "two words"]
        })
    );
}

/// 未指定初始命令时保留旧的可空状态，不构造空 shell 命令。
#[test]
fn missing_command_stays_null() {
    assert!(ssh_initial_command_value(None).is_null());
}
