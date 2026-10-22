//! 命令块投影只接受有界、单行且未命中现有敏感命令过滤规则的 UI 文本。
//!
//! @author kongweiguang

use kerminal_application::sessions::{
    CommandBlockText, CommandBlockTextUnknownReason, MAX_COMMAND_BLOCK_TEXT_BYTES,
    UiInputProjection, UiSessionBinding, UiTerminalCommandSubmissionRequest,
    UiTerminalInputRequest, classify_command_block_text, is_plain_command_enter,
};
use kerminal_domain::identity::{PaneId, SessionId, TabId};
use kerminal_domain::sessions::SessionOwner;
use kerminal_terminal::input::{
    InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyAlternates,
    TerminalKeyEvent,
};

/// 用真实领域历史过滤器固定安全正文与未知正文的边界，避免 UI 层重复实现过滤逻辑。
///
/// @author kongweiguang
#[test]
fn command_text_is_bounded_and_uses_history_sensitive_filter() {
    assert_eq!(
        classify_command_block_text("  echo safe  "),
        CommandBlockText::Known("echo safe".to_owned())
    );
    let known = classify_command_block_text("echo synthetic-secret");
    assert!(!format!("{known:?}").contains("synthetic-secret"));
    assert_eq!(
        classify_command_block_text("curl -H 'Authorization: Bearer abc'"),
        CommandBlockText::Unknown(CommandBlockTextUnknownReason::SensitiveOrUnsafe)
    );
    assert_eq!(
        classify_command_block_text("echo one\necho two"),
        CommandBlockText::Unknown(CommandBlockTextUnknownReason::SensitiveOrUnsafe)
    );
    assert_eq!(
        classify_command_block_text(&"x".repeat(MAX_COMMAND_BLOCK_TEXT_BYTES + 1)),
        CommandBlockText::Unknown(CommandBlockTextUnknownReason::SensitiveOrUnsafe)
    );
    assert_eq!(
        classify_command_block_text("  \t  "),
        CommandBlockText::Unknown(CommandBlockTextUnknownReason::SensitiveOrUnsafe)
    );
}

/// 只有明确且无修饰的 Enter 可关联 UI 命令正文，paste/快捷键不得冒充提交。
///
/// @author kongweiguang
#[test]
fn plain_enter_predicate_rejects_ambiguous_input_events() {
    assert!(is_plain_command_enter(&TerminalInputEvent::Key(
        TerminalKeyEvent {
            key: TerminalKey::Enter,
            phase: KeyPhase::Press,
            modifiers: InputModifiers::default(),
        }
    )));
    assert!(is_plain_command_enter(&TerminalInputEvent::Text(
        "\r".to_owned()
    )));
    assert!(!is_plain_command_enter(&TerminalInputEvent::Paste(
        "echo run\n".to_owned()
    )));
    assert!(!is_plain_command_enter(&TerminalInputEvent::Key(
        TerminalKeyEvent {
            key: TerminalKey::Enter,
            phase: KeyPhase::Press,
            modifiers: InputModifiers {
                control: true,
                ..InputModifiers::default()
            },
        }
    )));
    assert!(!is_plain_command_enter(&TerminalInputEvent::KeyWithText {
        event: TerminalKeyEvent {
            key: TerminalKey::Enter,
            phase: KeyPhase::Press,
            modifiers: InputModifiers::default(),
        },
        text: "\r".to_owned(),
        alternates: TerminalKeyAlternates {
            shifted: Some('c'),
            ..TerminalKeyAlternates::default()
        },
    }));
}

/// Debug 格式化不应把可信候选或 UI 输入事件复制到日志与诊断文本。
///
/// @author kongweiguang
#[test]
fn submission_debug_redacts_command_and_input() {
    let secret = "synthetic-secret-marker";
    let submission = UiTerminalCommandSubmissionRequest {
        input: UiTerminalInputRequest {
            binding: UiSessionBinding {
                session_id: SessionId::new(),
                owner: SessionOwner::Ui {
                    tab_id: TabId::new(),
                    pane_id: PaneId::new(),
                },
                session_generation: 1,
            },
            expected_context: None,
            projection: UiInputProjection::Tracked,
            event: TerminalInputEvent::Text(secret.to_owned()),
        },
        command: secret.to_owned(),
    };

    let debug = format!("{submission:?}");
    assert!(!debug.contains(secret));
}
