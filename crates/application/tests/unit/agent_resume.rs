//! Agent provider resume launch mapping tests.
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

use kerminal_domain::{agents::AgentProvider, sessions::LocalSessionTarget};

use crate::PublicErrorCode;

use super::{
    PI_AGENT_RESUME_COMMAND, builtin_resume_argv, initial_provider_session,
    require_saved_resume_command, shell_command_target,
};

/// 固定恢复元数据与新建 provider 能力保持兼容，Custom 不从配置猜测可恢复性。
#[test]
fn initial_provider_metadata_uses_frozen_resume_defaults() {
    let codex = initial_provider_session(AgentProvider::Codex);
    assert!(codex.resume_supported);
    assert_eq!(codex.resume_command.as_deref(), Some("codex resume --last"));

    let claude = initial_provider_session(AgentProvider::Claude);
    assert!(claude.resume_supported);
    assert_eq!(claude.resume_command.as_deref(), Some("claude --continue"));

    let pi = initial_provider_session(AgentProvider::Pi);
    assert!(pi.resume_supported);
    assert_eq!(pi.resume_command.as_deref(), Some(PI_AGENT_RESUME_COMMAND));

    let custom = initial_provider_session(AgentProvider::Custom);
    assert!(!custom.resume_supported);
    assert_eq!(custom.resume_command, None);
}

/// Provider 自己保存的 ID 作为单个 argv 优先于默认的 last/continue 恢复模式。
#[test]
fn provider_identity_maps_to_one_argument() {
    assert_eq!(
        builtin_resume_argv(AgentProvider::Codex, Some("thread-42"))
            .expect("Codex has a frozen id mapping"),
        (
            "codex".to_owned(),
            vec!["resume".to_owned(), "thread-42".to_owned()]
        )
    );
    assert_eq!(
        builtin_resume_argv(AgentProvider::Claude, Some("session-42"))
            .expect("Claude has a frozen id mapping"),
        (
            "claude".to_owned(),
            vec!["--resume".to_owned(), "session-42".to_owned()]
        )
    );
}

/// 未有冻结 Pi ID 语义时明确拒绝，不把 ID 拼入不支持的命令。
#[test]
fn pi_identity_is_not_guessed() {
    assert!(builtin_resume_argv(AgentProvider::Pi, Some("session-42")).is_err());
}

/// 旧版 shell 命令保持为一个原始参数，避免重新分词改变引号、变量和中文文本。
#[test]
fn saved_shell_command_is_passed_as_one_argument() {
    let target = local_target("bash", &["--norc"]);
    let command = "claude --continue; printf '%s' '你好 $HOME'";
    let (program, args) = shell_command_target(&target, command)
        .expect("known Bash profile can carry a raw saved command");

    assert_eq!(program, "bash");
    assert_eq!(
        args,
        vec!["--norc".to_owned(), "-lc".to_owned(), command.to_owned()]
    );
}

/// Windows 恢复 shell 命令沿用受管 Agent 的退出规范，只清理参数副本中的 NoExit。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
fn saved_powershell_resume_removes_noexit_without_mutating_profile() {
    let target = local_target(
        "pwsh.exe",
        &[
            "-NoE",
            "--noex",
            "/NOEXI",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
        ],
    );
    let command = "claude --continue; printf '%s' '你好 $HOME'";

    let (program, args) = shell_command_target(&target, command)
        .expect("known PowerShell profile can carry a raw saved command");

    assert_eq!(program, "pwsh.exe");
    assert_eq!(
        args,
        [
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-NoLogo",
            "-Command",
            command,
        ]
    );
    assert_eq!(
        target.args,
        [
            OsString::from("-NoE"),
            OsString::from("--noex"),
            OsString::from("/NOEXI"),
            OsString::from("-NoProfile"),
            OsString::from("-ExecutionPolicy"),
            OsString::from("Bypass"),
        ],
        "the saved ordinary shell profile remains unchanged"
    );
}

/// 已进入命令模式的 profile 或未知 shell 必须失败，不能嵌套或猜测解释器语义。
#[test]
fn saved_shell_command_rejects_ambiguous_profiles() {
    let command_mode = local_target("bash", &["-c", "echo existing"]);
    assert!(shell_command_target(&command_mode, "echo resume").is_err());

    let unknown = local_target("custom-shell", &[]);
    assert!(shell_command_target(&unknown, "echo resume").is_err());
}

/// Runtime 不得从 provider 默认策略补出缺失的 builtin 命令；Custom 仍以 Unsupported 明确拒绝。
///
/// @author kongweiguang
#[test]
fn resume_requires_persisted_command_for_supported_providers() {
    let mut codex = initial_provider_session(AgentProvider::Codex);
    codex.resume_command = None;
    let corrupt = require_saved_resume_command(&codex)
        .expect_err("supported builtin metadata must persist its command");
    assert_eq!(corrupt.public_code(), PublicErrorCode::ConfigurationCorrupt);

    let custom = initial_provider_session(AgentProvider::Custom);
    let unsupported = require_saved_resume_command(&custom)
        .expect_err("Custom without a saved command is not resumable");
    assert_eq!(unsupported.public_code(), PublicErrorCode::Unsupported);
}

/// 构造只包含测试输入的 profile，不读取用户 shell 或机器配置。
fn local_target(program: &str, args: &[&str]) -> LocalSessionTarget {
    LocalSessionTarget {
        program: OsString::from(program),
        args: args.iter().map(OsString::from).collect(),
        working_directory: Some(PathBuf::from("C:/agent-workspace")),
        environment: BTreeMap::new(),
    }
}
