//! 片段文件适配器内部的路径策略与读回确认测试。
//!
//! @author kongweiguang

use std::{fs, path::Path};

use kerminal_application::{PublicErrorCode, commands::SnippetTransferFileConfirmation};
use tempfile::tempdir;

use super::{
    CommitMode, export_destination, export_worker_join_error, file_matches,
    reconcile_unknown_export_commit,
};

/// 新目标按 create-new 提交，避免路径检查后出现的并发文件被覆盖。
#[test]
fn new_export_target_uses_create_new_mode() {
    let directory = tempdir().expect("temporary directory");
    let target = directory.path().join("snippets.json");

    let (resolved, mode) = export_destination(&target).expect("export destination");

    assert_eq!(resolved, canonical_child(directory.path(), "snippets.json"));
    assert!(matches!(mode, CommitMode::CreateNew));
}

/// 已有普通文件采用同目录原子替换，但类型改变或符号链接均不能作为目标。
#[test]
fn existing_export_target_uses_replace_mode_and_rejects_non_files() {
    let directory = tempdir().expect("temporary directory");
    let target = directory.path().join("snippets.json");
    fs::write(&target, b"existing").expect("write existing target");

    let (_, mode) = export_destination(&target).expect("existing export destination");
    assert!(matches!(mode, CommitMode::ReplaceExisting));

    let folder_target = directory.path().join("folder");
    fs::create_dir(&folder_target).expect("create folder target");
    assert!(export_destination(&folder_target).is_err());
}

/// 只有目标仍是普通文件且完整字节一致时，未知提交才有可验证成功证据。
#[test]
fn readback_requires_exact_regular_file_contents() {
    let directory = tempdir().expect("temporary directory");
    let target = directory.path().join("snippets.json");
    fs::write(&target, b"expected").expect("write target");

    assert!(file_matches(&target, b"expected", 32));
    assert!(!file_matches(&target, b"different", 32));
    assert!(!file_matches(&target, b"expected", 4));
    assert!(!file_matches(Path::new("missing.json"), b"expected", 32));
}

/// 目标未能证明提交时保留唯一完整的 staging 副本，且不改写原目标。
///
/// @author kongweiguang
#[test]
fn unknown_unconfirmed_export_retains_recovery_copy() {
    let directory = tempdir().expect("temporary directory");
    let target = directory.path().join("snippets.json");
    let staging = directory.path().join(".kerminal-snippets-test.tmp");
    let source = br#"{"schemaVersion":1,"snippets":[{"title":"new"}]}"#;
    fs::write(&target, b"old export").expect("write old target");
    fs::write(&staging, source).expect("write staged export");

    let error = reconcile_unknown_export_commit(&staging, &target, source, 128)
        .expect_err("mismatched destination remains unknown");

    assert_eq!(error.public_code(), PublicErrorCode::OutcomeUnknown);
    assert_eq!(
        fs::read(&target).expect("read original target"),
        b"old export"
    );
    assert_eq!(fs::read(&staging).expect("preserved recovery copy"), source);
}

/// 目标逐字节匹配导出内容时返回读回确认，并清理可能残留的 staging 副本。
///
/// @author kongweiguang
#[test]
fn unknown_export_with_exact_readback_confirms_and_cleans_stage() {
    let directory = tempdir().expect("temporary directory");
    let target = directory.path().join("snippets.json");
    let staging = directory.path().join(".kerminal-snippets-test.tmp");
    let source = br#"{"schemaVersion":1,"snippets":[{"title":"new"}]}"#;
    fs::write(&target, source).expect("write committed target");
    fs::write(&staging, source).expect("write leftover stage");

    let receipt = reconcile_unknown_export_commit(&staging, &target, source, 128)
        .expect("exact target readback confirms export");

    assert_eq!(receipt.bytes_written, source.len());
    assert_eq!(
        receipt.confirmation,
        SnippetTransferFileConfirmation::ReadBackConfirmed
    );
    assert!(!staging.exists());
}

/// 阻塞写入线程 panic 后按未知提交分类，供调用方回读而不是盲目重试。
///
/// @author kongweiguang
#[tokio::test]
async fn export_worker_join_error_is_outcome_unknown() {
    let join_error = tokio::task::spawn_blocking(|| panic!("test worker panic"))
        .await
        .expect_err("worker panic yields JoinError");

    let error = export_worker_join_error(join_error);

    assert_eq!(error.public_code(), PublicErrorCode::OutcomeUnknown);
}

/// 保持目标比较使用规范化的现有父目录，避免相对路径受临时工作目录影响。
fn canonical_child(parent: &Path, name: &str) -> std::path::PathBuf {
    parent.canonicalize().expect("canonical parent").join(name)
}
