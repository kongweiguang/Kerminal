//! Agent 连续性触发信号的原子发布与失败清理回归。
//!
//! 这些测试只触碰 `tempfile` 提供的本轮临时目录，验证空文件既是完整信号、
//! 又不会在碰撞或发布失败时覆盖/删除目录里已有的文件。
//!
//! @author kongweiguang

use std::{fs, io};

use super::{cleanup_continuity_stage_files, publish_continuity_stage_file};

/// 发布成功时目录中只出现最终 `trigger.stage`，`pending` 已经消失且正文为空。
///
/// 脚本只检查存在性，所以信号文件必须在 rename 完成后才可见，且不能带任何正文。
///
/// @author kongweiguang
#[test]
fn publishes_empty_signal_only_at_final_path() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("trigger.stage");

    publish_continuity_stage_file(&path).expect("publish empty signal");

    assert_eq!(fs::read(&path).expect("read signal"), Vec::<u8>::new());
    assert!(!directory.path().join("trigger.pending").exists());
}

/// `pending` 已被其它内容占用时直接失败，不删除也不改写该文件，也不产生最终信号。
///
/// 这保证清理和发布都以本轮 `create_new` 成功为前提，不把预存路径当作自己的文件。
///
/// @author kongweiguang
#[test]
fn pending_collision_preserves_preexisting_file() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("trigger.stage");
    let pending = directory.path().join("trigger.pending");
    fs::write(&pending, b"preexisting").expect("seed pending file");

    let error = publish_continuity_stage_file(&path).expect_err("collision must fail");

    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        fs::read(&pending).expect("pending survives"),
        b"preexisting"
    );
    assert!(!path.exists());
}

/// 最终路径已存在时在创建 pending 之前拒绝，既不改写目标也不遗留本轮文件。
///
/// Unix 的 rename 默认会覆盖目标，因此必须在重启发布前显式检查存在性。
///
/// @author kongweiguang
#[test]
fn target_collision_preserves_preexisting_target() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("trigger.stage");
    fs::write(&path, b"preexisting").expect("seed target file");

    let error = publish_continuity_stage_file(&path).expect_err("collision must fail");

    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&path).expect("target survives"), b"preexisting");
    assert!(!directory.path().join("trigger.pending").exists());
}

/// 清理只删除本轮目录里约定的 stage/pending，然后移除已空的运行目录。
///
/// @author kongweiguang
#[test]
fn cleanup_removes_own_files_and_empty_directory() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path().join("run");
    fs::create_dir(&root).expect("run directory");
    fs::write(root.join("trigger.stage"), b"stage").expect("seed stage");
    fs::write(root.join("trigger.pending"), b"pending").expect("seed pending");

    cleanup_continuity_stage_files(&root).expect("cleanup own run directory");

    assert!(!root.exists());
}

/// 发布成功后脚本尚未消费信号就失败时，外层仍以同一独占运行目录清走 stage 并移除空目录。
///
/// 这保证“触发文件已创建、随后关闭/提交失败”的路径不会遗留本轮文件。
///
/// @author kongweiguang
#[test]
fn published_signal_is_removed_by_run_directory_cleanup() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path().join("run");
    fs::create_dir(&root).expect("run directory");
    let path = root.join("trigger.stage");
    publish_continuity_stage_file(&path).expect("publish empty signal");
    assert!(path.exists());

    cleanup_continuity_stage_files(&root).expect("cleanup after publish");

    assert!(!root.exists());
}

/// 目录里出现非本轮文件时清理必须报错，保留该文件且不升级为递归删除。
///
/// 运行目录碰撞本应失败，但清理是最后一道防线：一旦边界内多出东西，只能交给人工处理。
///
/// @author kongweiguang
#[test]
fn cleanup_refuses_non_empty_directory_and_preserves_unrelated_file() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path().join("run");
    fs::create_dir(&root).expect("run directory");
    fs::write(root.join("trigger.stage"), b"stage").expect("seed stage");
    fs::write(root.join("unrelated.txt"), b"keep").expect("seed unrelated file");

    let error = cleanup_continuity_stage_files(&root).expect_err("non-empty directory must fail");

    assert!(root.exists());
    assert_eq!(
        fs::read(root.join("unrelated.txt")).expect("unrelated survives"),
        b"keep"
    );
    assert!(!root.join("trigger.stage").exists());
}

/// 成功流程结束后目录里两个约定文件都已不在时，清理仍应成功并能移除空目录。
///
/// 验收路径可能在失败回收、脚本自行删除或重复清理时遇到缺失文件，缺失不是错误。
///
/// @author kongweiguang
#[test]
fn cleanup_accepts_missing_trigger_files() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path().join("run");
    fs::create_dir(&root).expect("run directory");

    cleanup_continuity_stage_files(&root).expect("cleanup empty run directory");

    assert!(!root.exists());
}
