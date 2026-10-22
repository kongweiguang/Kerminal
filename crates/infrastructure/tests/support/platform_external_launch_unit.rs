//! Windows URL handler 命令生成与所有权判定测试，不写入真实用户注册表。  
//!
//! @author kongweiguang

use super::{command_executable, command_targets_executable, protocol_command};

/// Windows 协议命令完整引用可执行路径和 `%1`，含空格路径仍保持参数边界。
#[test]
fn protocol_command_quotes_the_executable_and_uri_argument() {
    assert_eq!(
        protocol_command(r"C:\Program Files\Kerminal\kerminal.exe").expect("valid executable path"),
        r#""C:\Program Files\Kerminal\kerminal.exe" "%1""#
    );
    assert!(protocol_command("C:\\unsafe\"path.exe").is_err());
}

/// 仅精确指向当前 executable 的 quoted command 被视为 Kerminal 所有，避免抢占其他 handler。
#[test]
fn protocol_owner_comparison_is_case_and_separator_insensitive() {
    let command = r#""C:\Program Files\Kerminal\kerminal.exe" "%1""#;
    assert_eq!(
        command_executable(command),
        Some(r"C:\Program Files\Kerminal\kerminal.exe")
    );
    assert!(command_targets_executable(
        command,
        r"c:/program files/kerminal/KERMINAL.EXE"
    ));
    assert!(!command_targets_executable(
        command,
        r"C:\Program Files\Other\kerminal.exe"
    ));
    assert!(!command_targets_executable(
        r"C:\Program Files\Kerminal\kerminal.exe %1",
        r"C:\Program Files\Kerminal\kerminal.exe"
    ));
}
