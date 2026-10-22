//! 命令提交历史只记录可证明的输入边界，不把终端写入误报为 shell 完成。
//!
//! @author kongweiguang

use super::*;
use kerminal_domain::{
    connections::RemoteShellDialect,
    identity::{PaneId, SessionId, TabId},
    sessions::{LocalSessionTarget, SessionTarget, TerminalSize},
};
use std::{collections::BTreeMap, ffi::OsString};

#[path = "command_submission_runtime.rs"]
mod runtime;

/// 快命令可能在 writer ack 返回前产生下一个 prompt，但输入代次仍能证明本次 Enter 已接受。
#[test]
fn ui_submission_accepts_a_new_prompt_epoch_after_exact_writer_ack() {
    let session_id = session_id();
    let owner = ui_owner();
    let expected = TerminalCommandContextVersion {
        session_generation: 3,
        prompt_epoch: 8,
        input_generation: 21,
    };
    let before = context(
        session_id.clone(),
        owner.clone(),
        expected,
        TerminalCommandAvailability::Available,
        false,
    );
    let mut acknowledgement = before.clone();
    acknowledgement.prompt_epoch += 1;
    acknowledgement.input_generation += 1;
    acknowledgement.prompt_ready = true;
    acknowledgement.prompt_line_empty = true;
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: owner.clone(),
        session_generation: expected.session_generation,
    };
    let input = UiTerminalInputRequest {
        binding,
        expected_context: Some(expected),
        projection: UiInputProjection::Tracked,
        event: enter_event(),
    };

    assert!(ui_submission_is_proven(
        &input,
        &before,
        &acknowledgement,
        &session(session_id, owner),
        expected,
    ));

    let mut empty_prompt_before = before.clone();
    empty_prompt_before.prompt_line_empty = true;
    assert!(!ui_submission_is_proven(
        &input,
        &empty_prompt_before,
        &acknowledgement,
        &session(
            input.binding.session_id.clone(),
            input.binding.owner.clone()
        ),
        expected,
    ));
}

/// 不精确的输入代次、重连代次或 owner 不能成为 User 历史的依据。
#[test]
fn ui_submission_rejects_stale_or_ambiguous_acknowledgements() {
    let session_id = session_id();
    let owner = ui_owner();
    let expected = TerminalCommandContextVersion {
        session_generation: 3,
        prompt_epoch: 8,
        input_generation: 21,
    };
    let before = context(
        session_id.clone(),
        owner.clone(),
        expected,
        TerminalCommandAvailability::Available,
        false,
    );
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: owner.clone(),
        session_generation: expected.session_generation,
    };
    let input = UiTerminalInputRequest {
        binding,
        expected_context: Some(expected),
        projection: UiInputProjection::Tracked,
        event: enter_event(),
    };
    let mut acknowledgement = before.clone();
    acknowledgement.input_generation += 2;
    assert!(!ui_submission_is_proven(
        &input,
        &before,
        &acknowledgement,
        &session(session_id.clone(), owner.clone()),
        expected,
    ));

    acknowledgement = before.clone();
    acknowledgement.session_generation += 1;
    assert!(!ui_submission_is_proven(
        &input,
        &before,
        &acknowledgement,
        &session(session_id.clone(), owner.clone()),
        expected,
    ));

    acknowledgement = before.clone();
    acknowledgement.input_generation += 1;
    acknowledgement.owner = SessionOwner::Mcp;
    assert!(!ui_submission_is_proven(
        &input,
        &before,
        &acknowledgement,
        &session(session_id, owner),
        expected,
    ));
}

/// MCP 写入只有一个显式终止符的普通单行可进入历史候选，原始输入仍由调用方照常发送。
#[test]
fn mcp_history_candidate_requires_one_printable_complete_line() {
    assert_eq!(single_printable_mcp_command("pwd\r"), Some("pwd"));
    assert_eq!(
        single_printable_mcp_command("echo 中文\n"),
        Some("echo 中文")
    );
    assert_eq!(single_printable_mcp_command("pwd"), None);
    assert_eq!(single_printable_mcp_command("echo one\necho two\n"), None);
    assert_eq!(
        single_printable_mcp_command("echo \u{1b}[31mred\u{1b}[0m\n"),
        None
    );
    assert_eq!(single_printable_mcp_command("echo\0bad\n"), None);
    assert_eq!(single_printable_mcp_command("\n"), None);
    assert_eq!(
        single_printable_mcp_command(&format!("{}\n", "x".repeat(4_001))),
        None
    );
}

/// MCP 只有完整、同目标且准确递增一次的 writer admission 才有历史记录资格。
#[test]
fn mcp_history_requires_same_writer_complete_clean_shell_admission() {
    let session_id = session_id();
    let admission = admission(session_id.clone(), SessionOwner::Mcp);
    let payload_bytes = "pwd\r".len();

    assert!(mcp_history_admission_is_proven(
        Some(&admission),
        &session_id,
        payload_bytes,
        payload_bytes,
    ));
    assert!(!mcp_history_admission_is_proven(
        None,
        &session_id,
        payload_bytes,
        payload_bytes,
    ));
    assert!(!mcp_history_admission_is_proven(
        Some(&admission),
        &session_id,
        payload_bytes - 1,
        payload_bytes,
    ));

    let mut invalid = admission.clone();
    invalid.accepted_input_generation += 1;
    assert!(!mcp_history_admission_is_proven(
        Some(&invalid),
        &session_id,
        payload_bytes,
        payload_bytes,
    ));
    invalid = admission.clone();
    invalid.prompt_ready = false;
    assert!(!mcp_history_admission_is_proven(
        Some(&invalid),
        &session_id,
        payload_bytes,
        payload_bytes,
    ));
    invalid = admission.clone();
    invalid.prompt_line_empty = false;
    assert!(!mcp_history_admission_is_proven(
        Some(&invalid),
        &session_id,
        payload_bytes,
        payload_bytes,
    ));
    invalid = admission.clone();
    invalid.shell_integration_enabled = false;
    assert!(!mcp_history_admission_is_proven(
        Some(&invalid),
        &session_id,
        payload_bytes,
        payload_bytes,
    ));
    invalid = admission.clone();
    invalid.shell_integration_observed = false;
    assert!(!mcp_history_admission_is_proven(
        Some(&invalid),
        &session_id,
        payload_bytes,
        payload_bytes,
    ));
    invalid = admission.clone();
    invalid.alternate_screen = true;
    assert!(!mcp_history_admission_is_proven(
        Some(&invalid),
        &session_id,
        payload_bytes,
        payload_bytes,
    ));
    invalid = admission.clone();
    invalid.shell_dialect = RemoteShellDialect::Unknown;
    assert!(!mcp_history_admission_is_proven(
        Some(&invalid),
        &session_id,
        payload_bytes,
        payload_bytes,
    ));
    invalid = admission.clone();
    invalid.lifecycle = SessionLifecycle::Exited;
    assert!(!mcp_history_admission_is_proven(
        Some(&invalid),
        &session_id,
        payload_bytes,
        payload_bytes,
    ));
    invalid = admission;
    invalid.session_id = SessionId::parse("other-session").expect("fixture ID is valid");
    assert!(!mcp_history_admission_is_proven(
        Some(&invalid),
        &session_id,
        payload_bytes,
        payload_bytes,
    ));
}

/// 读取会话目标只用于组装历史字段，并必须仍匹配 writer admission 中的 session/owner/lifecycle。
#[test]
fn history_metadata_must_still_match_the_admitted_writer_target() {
    let session_id = session_id();
    let owner = SessionOwner::Mcp;
    let admission = admission(session_id.clone(), owner.clone());
    let matching = session(session_id.clone(), owner.clone());
    assert!(history_session_matches_admission(&matching, &admission));

    let wrong_owner = session(session_id.clone(), ui_owner());
    assert!(!history_session_matches_admission(&wrong_owner, &admission));
    let wrong_id = session(
        SessionId::parse("other-session").expect("fixture ID is valid"),
        owner,
    );
    assert!(!history_session_matches_admission(&wrong_id, &admission));
    let mut closed = matching;
    closed.lifecycle = SessionLifecycle::Closed;
    assert!(!history_session_matches_admission(&closed, &admission));
}

/// 组合 Enter 仍原样写入终端，但不被误归因为普通 shell 行提交。
#[test]
fn modified_enter_is_not_history_submission_evidence() {
    let modifiers = [
        kerminal_terminal::input::InputModifiers {
            shift: true,
            ..Default::default()
        },
        kerminal_terminal::input::InputModifiers {
            alt: true,
            ..Default::default()
        },
        kerminal_terminal::input::InputModifiers {
            control: true,
            ..Default::default()
        },
        kerminal_terminal::input::InputModifiers {
            super_key: true,
            ..Default::default()
        },
    ];

    for modifiers in modifiers {
        let event = TerminalInputEvent::Key(kerminal_terminal::input::TerminalKeyEvent {
            key: TerminalKey::Enter,
            phase: KeyPhase::Press,
            modifiers,
        });
        assert!(!is_prompt_submit_event(&event));
    }
    assert!(is_prompt_submit_event(&enter_event()));
}

/// 可写的 CR/LF 脚本保留，但 ANSI、NUL 与其它控制字符 fail closed。
#[test]
fn history_text_allows_multiline_scripts_without_terminal_controls() {
    assert!(is_history_text("cd /tmp\nprintf 'ready'"));
    assert!(!is_history_text("echo \u{1b}[31mred"));
    assert!(!is_history_text("echo\0bad"));
    assert!(!is_history_text("  \n  "));
    let sensitive = NormalizedCommandHistory::new("curl -H 'Authorization: Bearer fixture-token'")
        .expect("sensitive fixture is still syntactically valid history text");
    assert!(sensitive.sensitive_skip_reason().is_some());
}

/// 统一使用显式 UI Enter 按下事件，避免粘贴换行混成命令提交证据。
fn enter_event() -> TerminalInputEvent {
    TerminalInputEvent::Key(kerminal_terminal::input::TerminalKeyEvent {
        key: TerminalKey::Enter,
        phase: KeyPhase::Press,
        modifiers: Default::default(),
    })
}

/// 构造可信上下文投影；调用方仍需单独确认版本与历史记录资格。
fn context(
    session_id: SessionId,
    owner: SessionOwner,
    version: TerminalCommandContextVersion,
    availability: TerminalCommandAvailability,
    prompt_line_empty: bool,
) -> TerminalCommandContext {
    TerminalCommandContext {
        session_id,
        owner,
        session_generation: version.session_generation,
        prompt_epoch: version.prompt_epoch,
        input_generation: version.input_generation,
        shell_dialect: RemoteShellDialect::Posix,
        prompt_ready: true,
        prompt_line_empty,
        alternate_screen: false,
        availability,
    }
}

/// 固定合法 fixture ID，避免测试结果依赖随机身份值。
fn session_id() -> SessionId {
    SessionId::parse("session-submission").expect("fixture session ID is valid")
}

/// 构造可信 runtime 生成的有效 admission，帮助测试每项历史资格约束独立失效。
fn admission(session_id: SessionId, owner: SessionOwner) -> TerminalWriteAdmission {
    TerminalWriteAdmission {
        session_id,
        owner,
        lifecycle: SessionLifecycle::Running,
        session_generation: 2,
        prompt_epoch: 5,
        input_generation_before: 10,
        accepted_input_generation: 11,
        shell_dialect: RemoteShellDialect::Posix,
        shell_integration_enabled: true,
        shell_integration_observed: true,
        prompt_ready: true,
        prompt_line_empty: true,
        alternate_screen: false,
    }
}

/// 构造拥有唯一 pane/tab 的 UI owner，验证记录目标不会从 active Tab 推断。
fn ui_owner() -> SessionOwner {
    SessionOwner::Ui {
        tab_id: TabId::parse("tab-submission").expect("fixture tab ID is valid"),
        pane_id: PaneId::parse("pane-submission").expect("fixture pane ID is valid"),
    }
}

/// 构造最小会话快照，只暴露本地目标与生命周期供用例证明检查。
fn session(session_id: SessionId, owner: SessionOwner) -> SessionInfo {
    SessionInfo {
        session_id,
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("bash"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        owner,
        size: TerminalSize::default(),
        lifecycle: SessionLifecycle::Running,
        exit_status: None,
    }
}
