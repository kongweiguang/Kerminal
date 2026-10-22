//! 文件编辑聚合的保存、冲突和关闭状态契约。
//!
//! @author kongweiguang

use std::path::PathBuf;

use kerminal_domain::files::editor::{
    CloseDecision, EditorPhase, EditorTransitionError, FileEditorSession, MAX_EDITOR_BYTES,
    SaveResolution,
};
use kerminal_domain::files::{
    FileEndpoint, FileRevision, LineEnding, TextDocument, WriteTextResult, WriteTextStrategy,
};

/// 用无 I/O 的本机端点构造文本文档，让领域状态测试不接触用户文件系统。
fn document(content: &str, readonly: bool, truncated: bool, revision: u64) -> TextDocument {
    document_with_line_ending(content, LineEnding::Lf, readonly, truncated, revision)
}

/// 为换行契约测试注入真实文件行尾，不依赖宿主操作系统默认格式。
fn document_with_line_ending(
    content: &str,
    line_ending: LineEnding,
    readonly: bool,
    truncated: bool,
    revision: u64,
) -> TextDocument {
    TextDocument {
        endpoint: FileEndpoint::Local {
            path: PathBuf::from("editor-contract.txt"),
        },
        content: content.to_owned(),
        line_ending,
        revision: FileRevision {
            size: revision,
            modified: Some(format!("revision-{revision}")),
            permissions: Some(0o644),
            content_digest: Some(format!("digest-{revision}")),
        },
        readonly,
        truncated,
    }
}

/// 生成适合当前保存的确认结果，模拟 application adapter 返回的新 revision。
fn write_result(revision: u64) -> WriteTextResult {
    WriteTextResult {
        bytes_written: revision,
        revision: FileRevision {
            size: revision,
            modified: Some(format!("revision-{revision}")),
            permissions: Some(0o644),
            content_digest: Some(format!("digest-{revision}")),
        },
    }
}

/// CompareRevision 保存成功后推进基线，后续编辑仍由当前缓冲计算 dirty。
#[test]
fn confirmed_save_advances_baseline_and_keeps_later_edits_dirty() {
    let mut session = FileEditorSession::open(document("before", false, false, 6));

    let command = session
        .begin_save("saved snapshot", SaveResolution::CompareRevision)
        .expect("修改过的文档可按 revision 保存");
    assert!(matches!(
        command.strategy,
        WriteTextStrategy::CompareRevision(FileRevision { size: 6, .. })
    ));

    session
        .save_succeeded(write_result(14))
        .expect("有效保存结果应完成迁移");

    assert_eq!(session.phase(), EditorPhase::Ready);
    assert_eq!(session.saved_content(), "saved snapshot");
    assert!(!session.is_dirty("saved snapshot"));
    assert!(session.is_dirty("new text typed while saving"));
}

/// CRLF 文件保存时将 GPUI 新插入的 LF 转回 CRLF，BOM 与编辑器 dirty 基线保持原样。
#[test]
fn crlf_save_preserves_bom_and_resets_dirty_to_the_editor_snapshot() {
    let initial = "\u{feff}first\r\nsecond";
    let edited = "\u{feff}first\r\nsecond\nthird";
    let mut session = FileEditorSession::open(document_with_line_ending(
        initial,
        LineEnding::Crlf,
        false,
        false,
        8,
    ));
    assert_eq!(session.saved_content(), initial);
    assert_eq!(session.line_ending(), LineEnding::Crlf);

    let command = session
        .begin_save(edited, SaveResolution::CompareRevision)
        .expect("用户新增行后应能保存编辑器正文");
    assert_eq!(command.content, "\u{feff}first\r\nsecond\r\nthird");
    session
        .save_succeeded(write_result(12))
        .expect("适配器确认写入后应推进编辑器基线");

    assert_eq!(session.saved_content(), edited);
    assert!(!session.is_dirty(edited));
    assert!(session.is_dirty("\u{feff}first\r\nsecond\nthird!"));
}

/// Mixed 文件保留原有 CRLF/LF 分隔，不为新增文本静默重写整个文件格式。
#[test]
fn mixed_line_endings_are_not_globally_normalized() {
    let initial = "first\r\nsecond\nthird";
    let edited = "first\r\ninserted\nsecond\nthird";
    let mut session = FileEditorSession::open(document_with_line_ending(
        initial,
        LineEnding::Mixed,
        false,
        false,
        8,
    ));
    assert_eq!(session.line_ending(), LineEnding::Mixed);

    let command = session
        .begin_save(edited, SaveResolution::CompareRevision)
        .expect("混合行尾文档的编辑可保存");
    assert_eq!(command.content, edited);
    session
        .save_succeeded(write_result(12))
        .expect("混合行尾的提交回执应推进编辑器基线");

    assert_eq!(session.saved_content(), edited);
    assert!(!session.is_dirty(edited));
}

/// LF 是空文件及 LF 文档的默认风格，编辑器粘贴的 CRLF 不应改变其保存格式。
#[test]
fn lf_documents_keep_lf_as_the_default_saved_style() {
    let mut session = FileEditorSession::open(document_with_line_ending(
        "",
        LineEnding::Lf,
        false,
        false,
        8,
    ));

    let command = session
        .begin_save("first\r\nsecond", SaveResolution::CompareRevision)
        .expect("新建空正文可按 LF 默认格式保存");

    assert_eq!(command.content, "first\nsecond");
}

/// CRLF 扩展会增加磁盘字节数，仍必须在生成写命令前执行 10 MiB 上限。
#[test]
fn serialized_crlf_expansion_cannot_exceed_editor_byte_limit() {
    let mut session = FileEditorSession::open(document_with_line_ending(
        "",
        LineEnding::Crlf,
        false,
        false,
        8,
    ));
    let many_newlines = "\n".repeat(MAX_EDITOR_BYTES / 2 + 1);

    assert_eq!(
        session.begin_save(&many_newlines, SaveResolution::CompareRevision),
        Err(EditorTransitionError::ContentTooLarge)
    );
    assert_eq!(session.phase(), EditorPhase::Ready);
}

/// 冲突后必须走显式覆盖分支，避免把同一旧 revision 静默重试。
#[test]
fn conflict_requires_explicit_overwrite_or_reload() {
    let mut session = FileEditorSession::open(document("before", false, false, 6));
    session
        .begin_save("mine", SaveResolution::CompareRevision)
        .expect("第一次保存应比较 revision");
    session.save_failed(true).expect("冲突结果应进入冲突态");

    assert_eq!(session.phase(), EditorPhase::Conflict);
    assert_eq!(
        session.begin_save("mine", SaveResolution::CompareRevision),
        Err(EditorTransitionError::ConflictResolutionRequired)
    );

    let overwrite = session
        .begin_save("mine", SaveResolution::OverwriteAfterConflict)
        .expect("用户确认后可显式覆盖");
    assert_eq!(overwrite.strategy, WriteTextStrategy::ExplicitOverwrite);
}

/// 未知提交保持 dirty 并禁止再次比较或覆盖，只有重新加载可同步真实 revision。
#[test]
fn unknown_save_result_requires_readback_before_any_new_write() {
    let mut session = FileEditorSession::open(document("before", false, false, 6));
    session
        .begin_save("possibly committed", SaveResolution::CompareRevision)
        .expect("dirty document should start its first save");
    session
        .save_outcome_unknown()
        .expect("unknown adapter result should enter a dedicated state");

    assert_eq!(session.phase(), EditorPhase::OutcomeUnknown);
    assert!(session.is_dirty("possibly committed"));
    assert_eq!(
        session.begin_save("possibly committed", SaveResolution::CompareRevision),
        Err(EditorTransitionError::OutcomeUnknownRequiresReload)
    );
    assert_eq!(
        session.begin_save("possibly committed", SaveResolution::OverwriteAfterConflict),
        Err(EditorTransitionError::OutcomeUnknownRequiresReload)
    );

    session
        .reload(document("possibly committed", false, false, 14))
        .expect("explicit readback should replace the uncertain baseline");
    assert_eq!(session.phase(), EditorPhase::Ready);
    assert!(!session.is_dirty("possibly committed"));
}

/// 只读和截断文档不可进入保存态，长度上限在创建写命令前强制执行。
#[test]
fn readonly_truncated_and_oversized_documents_cannot_be_saved() {
    let mut readonly = FileEditorSession::open(document("before", true, false, 6));
    assert_eq!(
        readonly.begin_save("changed", SaveResolution::CompareRevision),
        Err(EditorTransitionError::ReadOnly)
    );

    let mut truncated = FileEditorSession::open(document("prefix", false, true, 6));
    assert_eq!(
        truncated.begin_save("changed", SaveResolution::CompareRevision),
        Err(EditorTransitionError::ReadOnly)
    );

    let mut editable = FileEditorSession::open(document("before", false, false, 6));
    let oversized = "x".repeat(MAX_EDITOR_BYTES + 1);
    assert_eq!(
        editable.begin_save(&oversized, SaveResolution::CompareRevision),
        Err(EditorTransitionError::ContentTooLarge)
    );
    assert_eq!(editable.phase(), EditorPhase::Ready);
}

/// 脏状态由正文比较驱动；保存中拒绝并行保存，关闭等待异步事务结束。
#[test]
fn dirty_close_and_save_in_progress_have_explicit_decisions() {
    let mut session = FileEditorSession::open(document("before", false, false, 6));
    assert_eq!(session.close_decision("before"), CloseDecision::CloseNow);
    assert_eq!(
        session.close_decision("changed"),
        CloseDecision::ConfirmUnsavedChanges
    );

    session
        .begin_save("changed", SaveResolution::CompareRevision)
        .expect("首次修改可以开始保存");
    assert_eq!(
        session.close_decision("changed"),
        CloseDecision::WaitForSave
    );
    assert_eq!(
        session.begin_save("changed again", SaveResolution::CompareRevision),
        Err(EditorTransitionError::SaveInProgress)
    );
}

/// 显式重载用新读取文档整体替换基线并清除冲突，不能把脏内容自动带入新 revision。
#[test]
fn reload_replaces_conflicted_baseline_and_clears_dirty_state() {
    let mut session = FileEditorSession::open(document("before", false, false, 6));
    session
        .begin_save("mine", SaveResolution::CompareRevision)
        .expect("保存应进入 revision 比较");
    session.save_failed(true).expect("模拟远端 revision 冲突");
    session
        .reload(document("theirs", false, false, 9))
        .expect("冲突后允许用户主动重新加载");

    assert_eq!(session.phase(), EditorPhase::Ready);
    assert_eq!(session.saved_content(), "theirs");
    assert!(!session.is_dirty("theirs"));
    assert!(matches!(
        session.begin_save("mine", SaveResolution::OverwriteAfterConflict),
        Err(EditorTransitionError::OverwriteRequiresConflict)
    ));
}
