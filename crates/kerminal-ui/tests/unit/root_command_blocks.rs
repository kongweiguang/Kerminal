//! Root 命令块投影和有限预览策略测试。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_application::sessions::{
    CommandBlock, CommandBlockId, CommandBlockOutputIncompleteReason, CommandBlockOutputSnapshot,
    CommandBlockOutputState, CommandBlockOutputText, CommandBlockSnapshot, CommandBlockState,
    CommandBlockText, CommandBlockTextUnknownReason, CommandBlockUnknownReason,
};
use kerminal_domain::identity::{AgentSessionId, SessionId};

use crate::root::command_blocks::{
    CommandBlockContent, CommandBlockContentUnavailable, CommandBlockRuntime,
    main_command_block_text, project_blocks, project_command_block_output,
    snapshot_matches_session, snippet_command_block_text,
};

/// UI 元数据刷新不得把 shell parser 的命令内容误当成已经读取的终端输出。
#[test]
fn metadata_projection_never_invents_transcript_content() {
    let snapshot = snapshot(
        SessionId::new(),
        vec![CommandBlock {
            id: CommandBlockId {
                session_generation: 8,
                prompt_epoch: 3,
                ordinal: 7,
            },
            command: CommandBlockText::Known("printf secret-safe".to_owned()),
            state: CommandBlockState::Finished { exit_code: Some(0) },
        }],
    );

    let projected = project_blocks(snapshot);

    assert_eq!(projected.len(), 1);
    assert_eq!(
        projected[0].command,
        CommandBlockText::Known("printf secret-safe".to_owned())
    );
    assert!(projected[0].content == CommandBlockContent::NotRequested);
}

/// 无法安全展示的命令仍保留未知原因，不能为了卡片布局替换成空的已知命令。
#[test]
fn metadata_projection_preserves_unknown_command_and_execution_state() {
    let id = CommandBlockId {
        session_generation: 2,
        prompt_epoch: 5,
        ordinal: 1,
    };
    let snapshot = snapshot(
        SessionId::new(),
        vec![CommandBlock {
            id,
            command: CommandBlockText::Unknown(CommandBlockTextUnknownReason::SensitiveOrUnsafe),
            state: CommandBlockState::Unknown(CommandBlockUnknownReason::Disconnected),
        }],
    );

    let projected = project_blocks(snapshot);

    assert_eq!(projected[0].id, id);
    assert_eq!(
        projected[0].command,
        CommandBlockText::Unknown(CommandBlockTextUnknownReason::SensitiveOrUnsafe)
    );
    assert_eq!(
        projected[0].state,
        CommandBlockState::Unknown(CommandBlockUnknownReason::Disconnected)
    );
}

/// 应用返回的快照必须仍显式属于本次请求的 session，不能跨 pane 复用。
#[test]
fn snapshot_identity_must_match_requested_session() {
    let requested = SessionId::new();
    let other = SessionId::new();

    assert!(snapshot_matches_session(
        &snapshot(requested.clone(), Vec::new()),
        &requested
    ));
    assert!(!snapshot_matches_session(
        &snapshot(other, Vec::new()),
        &requested
    ));
}

/// 快照内容不得夹带另一代或重复 block ID，避免旧正文跨重连或重复操作。
#[test]
fn snapshot_rejects_mixed_generation_and_duplicate_blocks() {
    let session_id = SessionId::new();
    let mixed = snapshot(
        session_id.clone(),
        vec![block(CommandBlockId {
            session_generation: 7,
            prompt_epoch: 1,
            ordinal: 1,
        })],
    );
    assert!(!snapshot_matches_session(&mixed, &session_id));

    let id = CommandBlockId {
        session_generation: 8,
        prompt_epoch: 1,
        ordinal: 1,
    };
    let duplicate = snapshot(session_id.clone(), vec![block(id), block(id)]);
    assert!(!snapshot_matches_session(&duplicate, &session_id));
}

/// Transcript 状态保留采集边界；空的缺失正文不能变成可复制空输出。
#[test]
fn transcript_projection_preserves_status_and_missing_content() {
    let session_id = SessionId::new();
    let block_id = CommandBlockId {
        session_generation: 8,
        prompt_epoch: 4,
        ordinal: 9,
    };

    assert!(
        project_command_block_output(
            output_snapshot(
                session_id.clone(),
                block_id,
                CommandBlockOutputState::NotCaptured,
                None,
            ),
            &session_id,
            8,
            block_id,
        ) == Some(CommandBlockContent::Unavailable(
            CommandBlockContentUnavailable::NotCaptured
        ))
    );
    assert!(
        project_command_block_output(
            output_snapshot(
                session_id.clone(),
                block_id,
                CommandBlockOutputState::Capturing,
                None,
            ),
            &session_id,
            8,
            block_id,
        ) == Some(CommandBlockContent::Loading)
    );
    assert!(
        project_command_block_output(
            output_snapshot(
                session_id.clone(),
                block_id,
                CommandBlockOutputState::Evicted,
                None,
            ),
            &session_id,
            8,
            block_id,
        ) == Some(CommandBlockContent::Unavailable(
            CommandBlockContentUnavailable::Evicted
        ))
    );
    assert!(matches!(
        project_command_block_output(
            output_snapshot(
                session_id.clone(),
                block_id,
                CommandBlockOutputState::Incomplete(
                    CommandBlockOutputIncompleteReason::Disconnected,
                ),
                None,
            ),
            &session_id,
            8,
            block_id,
        ),
        Some(CommandBlockContent::Unavailable(
            CommandBlockContentUnavailable::Incomplete(_)
        ))
    ));
    assert!(
        project_command_block_output(
            output_snapshot(
                session_id.clone(),
                block_id,
                CommandBlockOutputState::Complete { exit_code: Some(0) },
                None,
            ),
            &session_id,
            8,
            block_id,
        ) == Some(CommandBlockContent::Unavailable(
            CommandBlockContentUnavailable::CaptureUnavailable
        ))
    );
    assert!(
        project_command_block_output(
            output_snapshot(
                session_id,
                block_id,
                CommandBlockOutputState::Complete { exit_code: Some(0) },
                None,
            ),
            &SessionId::new(),
            8,
            block_id,
        )
        .is_none()
    );
}

/// 只有 generation 对齐的输出快照才可进入卡片；迟到回执不能跨重连展示。
#[test]
fn transcript_projection_rejects_stale_block_generation() {
    let session_id = SessionId::new();
    let block_id = CommandBlockId {
        session_generation: 8,
        prompt_epoch: 2,
        ordinal: 3,
    };

    assert!(
        project_command_block_output(
            output_snapshot(
                session_id.clone(),
                block_id,
                CommandBlockOutputState::Incomplete(
                    CommandBlockOutputIncompleteReason::Disconnected,
                ),
                None,
            ),
            &session_id,
            9,
            block_id,
        )
        .is_none()
    );
}

/// 内容查询带回的 revision 必须保留到 UI 投影；无 revision 的正文不可用于复制或发送。
#[test]
fn transcript_projection_preserves_capture_revision_and_text_boundary() {
    let session_id = SessionId::new();
    let block_id = CommandBlockId {
        session_generation: 8,
        prompt_epoch: 4,
        ordinal: 2,
    };
    let text = Some(CommandBlockOutputText::from_capture(Arc::from(
        "captured output",
    )));

    let content = project_command_block_output(
        output_snapshot_with_revision(
            session_id.clone(),
            block_id,
            CommandBlockOutputState::Complete { exit_code: Some(0) },
            text.clone(),
            Some(17),
        ),
        &session_id,
        8,
        block_id,
    )
    .expect("matching output projects");
    assert!(
        content
            == (CommandBlockContent::Available {
                text: "captured output".to_owned(),
                capture_revision: Some(17),
                truncated: false,
                complete: true,
            })
    );

    let unversioned = project_command_block_output(
        output_snapshot_with_revision(
            session_id.clone(),
            block_id,
            CommandBlockOutputState::Complete { exit_code: Some(0) },
            text,
            None,
        ),
        &session_id,
        8,
        block_id,
    )
    .expect("matching output projects for display");
    assert!(
        unversioned
            == (CommandBlockContent::Available {
                text: "captured output".to_owned(),
                capture_revision: None,
                truncated: false,
                complete: true,
            })
    );
}

/// 新建片段预填复用 main 的命令块全文格式，并清理每行尾部空白。
#[test]
fn snippet_prefill_contains_command_and_captured_output() {
    let session_id = SessionId::new();
    let block_id = CommandBlockId {
        session_generation: 8,
        prompt_epoch: 4,
        ordinal: 2,
    };
    let snapshot = output_snapshot(
        session_id.clone(),
        block_id,
        CommandBlockOutputState::Complete { exit_code: Some(0) },
        Some(CommandBlockOutputText::from_capture(Arc::from(
            "first  \nsecond\n",
        ))),
    );

    assert_eq!(
        snippet_command_block_text(snapshot, &session_id, block_id, 8, Some(2), "printf safe")
            .expect("matching transcript becomes snippet text"),
        "$ printf safe\nfirst\nsecond"
    );
}

/// 缺少输出或回执属于其它 block 时不创建只有命令前缀的片段。
#[test]
fn snippet_prefill_rejects_missing_or_mismatched_output() {
    let session_id = SessionId::new();
    let block_id = CommandBlockId {
        session_generation: 8,
        prompt_epoch: 4,
        ordinal: 2,
    };
    assert!(
        snippet_command_block_text(
            output_snapshot(
                session_id.clone(),
                block_id,
                CommandBlockOutputState::NotCaptured,
                None,
            ),
            &session_id,
            block_id,
            8,
            Some(2),
            "printf safe",
        )
        .is_err()
    );

    let mut truncated = output_snapshot(
        session_id.clone(),
        block_id,
        CommandBlockOutputState::Complete { exit_code: Some(0) },
        Some(CommandBlockOutputText::from_capture(Arc::from("prefix"))),
    );
    truncated.truncated = true;
    assert!(
        snippet_command_block_text(truncated, &session_id, block_id, 8, Some(2), "printf safe",)
            .is_err()
    );

    let other_block = CommandBlockId {
        ordinal: 3,
        ..block_id
    };
    assert!(
        snippet_command_block_text(
            output_snapshot(
                session_id.clone(),
                other_block,
                CommandBlockOutputState::Complete { exit_code: Some(0) },
                Some(CommandBlockOutputText::from_capture(Arc::from("output"))),
            ),
            &session_id,
            block_id,
            8,
            Some(2),
            "printf safe",
        )
        .is_err()
    );
}

/// 即使 session 和 block 仍匹配，输出 revision 变化也不能用旧命令块文本创建片段。
#[test]
fn snippet_prefill_rejects_changed_capture_revision() {
    let session_id = SessionId::new();
    let block_id = CommandBlockId {
        session_generation: 8,
        prompt_epoch: 4,
        ordinal: 2,
    };
    let snapshot = output_snapshot_with_revision(
        session_id.clone(),
        block_id,
        CommandBlockOutputState::Complete { exit_code: Some(0) },
        Some(CommandBlockOutputText::from_capture(Arc::from("output"))),
        Some(2),
    );

    assert!(
        snippet_command_block_text(snapshot, &session_id, block_id, 8, Some(1), "printf safe",)
            .is_err()
    );
}

/// main 保留固定字符尾窗，避免超长 transcript 造成无界片段草稿。
#[test]
fn snippet_prefill_uses_main_output_tail_window() {
    let output = format!("{}tail", "x".repeat(128_000));
    let text = main_command_block_text("printf safe", &output);

    assert!(text.ends_with("tail"));
    assert_eq!(
        text.chars().count(),
        "$ printf safe\n".chars().count() + 128_000
    );
}

/// 未知提交风险绑定来源会话、区块与目标 Agent，不能因换目标或会话而误清除。
#[test]
fn uncertain_agent_submission_isolated_by_source_and_exact_target() {
    let mut runtime = CommandBlockRuntime::default();
    let source_session = SessionId::new();
    let other_session = SessionId::new();
    let target_agent = AgentSessionId::new();
    let other_agent = AgentSessionId::new();
    let block_id = CommandBlockId {
        session_generation: 8,
        prompt_epoch: 4,
        ordinal: 2,
    };

    runtime.mark_agent_submission_uncertain(&source_session, block_id, target_agent.clone());

    assert!(runtime.agent_submission_is_uncertain(&source_session, block_id, &target_agent));
    assert!(!runtime.agent_submission_is_uncertain(&source_session, block_id, &other_agent));
    assert!(!runtime.agent_submission_is_uncertain(&other_session, block_id, &target_agent));
}

/// 构造只有领域快照的 fixture，避免测试依赖终端、数据库或 GPUI 窗口。
fn snapshot(session_id: SessionId, blocks: Vec<CommandBlock>) -> CommandBlockSnapshot {
    CommandBlockSnapshot {
        session_id,
        session_generation: 8,
        revision: 13,
        blocks,
    }
}

/// 构造只有领域身份的卡片，不携带命令正文或输出内容。
fn block(id: CommandBlockId) -> CommandBlock {
    CommandBlock {
        id,
        command: CommandBlockText::Unknown(CommandBlockTextUnknownReason::NotCaptured),
        state: CommandBlockState::Running,
    }
}

/// 为边界测试构造不含秘密正文的应用 transcript 快照。
fn output_snapshot(
    session_id: SessionId,
    block_id: CommandBlockId,
    state: CommandBlockOutputState,
    text: Option<kerminal_application::sessions::CommandBlockOutputText>,
) -> CommandBlockOutputSnapshot {
    output_snapshot_with_revision(session_id, block_id, state, text, Some(2))
}

/// 构造可指定版本边界的输出快照，覆盖查询与 UI 之间的竞态契约。
fn output_snapshot_with_revision(
    session_id: SessionId,
    block_id: CommandBlockId,
    state: CommandBlockOutputState,
    text: Option<CommandBlockOutputText>,
    capture_revision: Option<u64>,
) -> CommandBlockOutputSnapshot {
    CommandBlockOutputSnapshot {
        session_id,
        block_id,
        state,
        capture_revision,
        text,
        captured_bytes: 0,
        returned_bytes: 0,
        truncated: false,
    }
}
