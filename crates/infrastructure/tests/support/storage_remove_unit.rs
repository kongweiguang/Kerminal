//! 文件删除结果阶段的私有回归测试。
//!
//! @author kongweiguang

use std::io;

use super::{AtomicCommitError, remove_file_with_parent_sync_using};

/// 删除前路径缺失必须保持为明确未提交，且不进入父目录同步阶段。
#[test]
fn missing_delete_target_is_not_committed() {
    let root = tempfile::tempdir().expect("创建隔离目录");
    let target = root.path().join("missing.toml");
    let sync_called = std::cell::Cell::new(false);

    let error = remove_file_with_parent_sync_using(&target, |_| {
        sync_called.set(true);
        Ok(())
    })
    .expect_err("缺失目标不能伪报删除成功");

    assert!(matches!(error, AtomicCommitError::NotCommitted(_)));
    assert!(!sync_called.get(), "未 unlink 时不应执行目录同步");
}

/// unlink 已发生而目录同步失败时必须标记未知，调用方不能把它当作安全重试的失败。
///
/// 用通用 I/O 错误注入同步失败，不额外引入会影响提交结果分类的错误类型。
#[test]
fn parent_sync_failure_after_unlink_is_commit_unknown() {
    let root = tempfile::tempdir().expect("创建隔离目录");
    let target = root.path().join("host.toml");
    std::fs::write(&target, "schema_version = 2\n").expect("写入合成配置");

    let error = remove_file_with_parent_sync_using(&target, |_| {
        Err(io::Error::other("synthetic sync failure"))
    })
    .expect_err("unlink 后同步失败必须保留未知结果分类");

    assert!(matches!(error, AtomicCommitError::CommitUnknown(_)));
    assert!(!target.exists(), "测试应确认 unlink 已发生");
}
