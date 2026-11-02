//! 冷重启验收回执与隔离输入边界回归。
//!
//! @author kongweiguang

use super::*;

/// 收尾成功前的临时回执不能被误认为可用于第二进程的完成回执。
///
/// @author kongweiguang
#[test]
fn prepared_receipt_is_published_only_by_finish() {
    let directory = tempfile::tempdir().expect("temporary directory");
    assert!(finish_prepare(directory.path()).is_err());
    fs::write(directory.path().join(PENDING_RECEIPT), b"test").expect("pending receipt");
    finish_prepare(directory.path()).expect("publish receipt");
    assert!(!directory.path().join(PENDING_RECEIPT).exists());
    assert_eq!(
        fs::read(directory.path().join(PREPARED_RECEIPT)).expect("prepared receipt"),
        b"test"
    );
}

/// 输出和来源必须显式指定；相对路径或缺失清单在任何文件创建之前拒绝。
///
/// @author kongweiguang
#[test]
fn restart_output_rejects_unbound_or_relative_paths() {
    assert!(validate_output_request(&None, None).is_err());
    assert!(
        validate_output_request(
            &Some(PathBuf::from("restart-relative")),
            Some(Path::new("manifest.json"))
        )
        .is_err()
    );
    assert!(
        resolve_previous_run(Path::new("restart-relative"), Path::new("manifest.json")).is_err()
    );
}

/// 配置复用只接受普通文件和目录，避免把特殊文件误认成隔离配置。
///
/// @author kongweiguang
#[test]
fn plain_tree_rejects_wrong_entry_kind() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let file = directory.path().join("file");
    fs::write(&file, b"test").expect("regular file");
    require_plain_tree(directory.path()).expect("plain tree");
    assert!(require_plain_entry(&file, true).is_err());
    assert!(require_plain_entry(directory.path(), false).is_err());
}
