//! 命令块视图的纯策略回归测试。
//!
//! @author kongweiguang

use kerminal_application::sessions::{
    CommandBlockId, CommandBlockState, CommandBlockText, CommandBlockTextUnknownReason,
};
use kerminal_domain::identity::AgentSessionId;

use super::super::command_blocks::AgentSubmissionUnknowns;
use crate::features::terminal_tools::command_blocks::{
    CommandBlockContent, CommandBlockProjection, MAX_COPY_BYTES, TRUNCATION_MARKER,
    TextHighlightRange, block_export_revision, block_export_text, capture_revision_matches,
    normalized_text_segments, should_accept_snapshot,
};

/// 新 session generation 允许 revision 重新开始，同代迟到快照必须被拒绝。
#[test]
fn snapshot_revision_is_monotonic_per_generation() {
    assert!(should_accept_snapshot(Some(4), 12, 4, 12));
    assert!(should_accept_snapshot(Some(4), 12, 4, 13));
    assert!(!should_accept_snapshot(Some(4), 12, 4, 11));
    assert!(should_accept_snapshot(Some(4), 12, 5, 1));
}

/// 只有命令和 transcript 都明确已知时才允许复制完整卡片正文。
#[test]
fn export_requires_known_command_and_loaded_transcript() {
    let mut block = projection(CommandBlockText::Known("printf safe".to_owned()), "output");
    assert_eq!(
        block_export_text(&block).as_deref(),
        Some("$ printf safe\noutput")
    );

    block.command = CommandBlockText::Unknown(CommandBlockTextUnknownReason::SensitiveOrUnsafe);
    assert_eq!(block_export_text(&block), None);

    block.command = CommandBlockText::Known("printf safe".to_owned());
    block.content = CommandBlockContent::NotRequested;
    assert_eq!(block_export_text(&block), None);
}

/// 输出只在带应用 capture revision 时可导出，防止按钮确认期间引用被替换的内容。
#[test]
fn export_requires_stable_capture_revision() {
    let mut block = projection(CommandBlockText::Known("printf safe".to_owned()), "output");
    assert_eq!(block_export_revision(&block), Some(2));
    assert!(capture_revision_matches(
        Some(2),
        block_export_revision(&block)
    ));
    assert!(!capture_revision_matches(
        Some(1),
        block_export_revision(&block)
    ));
    assert!(!capture_revision_matches(
        None,
        block_export_revision(&block)
    ));

    block.content = CommandBlockContent::Available {
        text: "output".to_owned(),
        capture_revision: None,
        truncated: false,
        complete: true,
    };
    assert_eq!(block_export_revision(&block), None);
    assert_eq!(block_export_text(&block), None);
}

/// 即使采集器只返回短前缀，复制、图片和 Agent 预览共享的 payload 也标出来源截断。
#[test]
fn export_marks_backend_truncated_output() {
    let mut block = projection(CommandBlockText::Known("cat file".to_owned()), "partial");
    block.content = CommandBlockContent::Available {
        text: "partial".to_owned(),
        capture_revision: Some(2),
        truncated: true,
        complete: true,
    };

    let exported = block_export_text(&block).expect("versioned transcript can be exported");
    assert!(exported.contains("转录已截断") || exported.contains("transcript was truncated"));
}

/// 不完整状态和真实原因进入复制 payload，避免 PNG/Agent 预览把前缀伪装为完整输出。
#[test]
fn export_marks_incomplete_output_reason() {
    let mut block = projection(CommandBlockText::Known("cat file".to_owned()), "partial");
    block.content = CommandBlockContent::Incomplete {
        text: "partial".to_owned(),
        capture_revision: Some(2),
        reason: "Connection disconnected".to_owned(),
        truncated: false,
    };

    let exported = block_export_text(&block).expect("versioned transcript can be exported");
    assert!(exported.contains("输出不完整") || exported.contains("Output is incomplete"));
    assert!(exported.contains("Connection disconnected"));
}

/// 未知 ACK 按来源区块和目标 Agent 持久保留，不因输出 revision 变化而解锁。
///
/// @author kongweiguang
#[test]
fn unknown_agent_submission_is_scoped_to_agent_and_survives_output_revision_changes() {
    let block = projection(CommandBlockText::Known("printf safe".to_owned()), "output");
    let selected_agent = AgentSessionId::new();
    let other_agent = AgentSessionId::new();
    let mut unknowns = AgentSubmissionUnknowns::default();
    unknowns.mark(block.id, selected_agent.clone());

    assert!(unknowns.agents_for(block.id).contains(&selected_agent));
    assert!(!unknowns.agents_for(block.id).contains(&other_agent));

    let mut refreshed = block.clone();
    refreshed.content = CommandBlockContent::Available {
        text: "new output after the previous send".to_owned(),
        capture_revision: Some(3),
        truncated: false,
        complete: true,
    };
    let live_blocks = [block.id].into_iter().collect();
    unknowns.retain_live_blocks(&live_blocks);
    assert_eq!(block_export_revision(&refreshed), Some(3));
    assert!(unknowns.agents_for(block.id).contains(&selected_agent));
}

/// 有界复制按 UTF-8 字符边界裁切，并在 payload 内保留明确的截断标记。
#[test]
fn export_is_bounded_and_utf8_safe() {
    let block = projection(
        CommandBlockText::Known("printf safe".to_owned()),
        &"中".repeat(MAX_COPY_BYTES),
    );
    let exported = block_export_text(&block).expect("known transcript can be exported");

    assert!(exported.len() <= MAX_COPY_BYTES);
    assert!(exported.ends_with(TRUNCATION_MARKER));
    assert!(exported.is_char_boundary(exported.len()));
}

/// 即使调用方传入小于截断标记的预算，也不得返回超过预算的剪贴板正文。
#[test]
fn export_respects_small_byte_budget() {
    let block = projection(CommandBlockText::Known("x".to_owned()), "输出内容");
    let exported = block_export_text(&block).expect("known transcript can be exported");
    let bounded = super::super::command_blocks::truncate_with_marker(&exported, 5);

    assert!(bounded.len() <= 5);
    assert!(bounded.is_char_boundary(bounded.len()));
}

/// 重叠或切入 UTF-8 字符的 highlight 范围应合并并校正，而不产生非法切片。
#[test]
fn highlight_ranges_are_merged_at_character_boundaries() {
    let segments = normalized_text_segments(
        "a中z",
        &[
            TextHighlightRange {
                start_byte: 2,
                end_byte: 5,
            },
            TextHighlightRange {
                start_byte: 1,
                end_byte: 3,
            },
        ],
    );

    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].text, "a");
    assert!(!segments[0].highlighted);
    assert_eq!(segments[1].text, "中z");
    assert!(segments[1].highlighted);
}

/// 构造仅用于 UI 策略测试的 projection，不依赖真实 session 或存储。
fn projection(command: CommandBlockText, output: &str) -> CommandBlockProjection {
    CommandBlockProjection {
        id: CommandBlockId {
            session_generation: 7,
            prompt_epoch: 3,
            ordinal: 2,
        },
        command,
        state: CommandBlockState::Finished { exit_code: Some(0) },
        content: CommandBlockContent::Available {
            text: output.to_owned(),
            capture_revision: Some(2),
            truncated: false,
            complete: true,
        },
    }
}
