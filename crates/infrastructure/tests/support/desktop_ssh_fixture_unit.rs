//! SSH 验收 seeder 的路径边界测试；只使用临时目录和无效演示字节。
//!
//! @author kongweiguang

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use super::{paths_overlap, validate_output_path, validate_source_file};

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    /// 测试使用独立临时目录，避免并行执行时共享 fixture 或触碰验收数据。
    /// @author kongweiguang
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after UNIX epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "kerminal-ssh-fixture-boundary-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create isolated fixture test directory");
        Self(path)
    }

    /// 暴露临时根供单个路径断言使用，不共享生产配置根。
    /// @author kongweiguang
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    /// 测试结束后删除自己创建的临时 fixture，不清理任何共享目录。
    /// @author kongweiguang
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 只接受显式源目录内规范化后的普通文件，避免外部绝对路径扩大读取范围。
/// @author kongweiguang
#[test]
fn source_file_must_canonicalize_inside_explicit_root() {
    let temporary = TemporaryDirectory::new();
    let source_root = temporary.path().join("task-private-source");
    fs::create_dir(&source_root).expect("create source root");
    let allowed = source_root.join("known-hosts.fixture");
    fs::write(&allowed, b"test-only-known-host-data\n").expect("write test fixture");
    let outside = temporary.path().join("outside.fixture");
    fs::write(&outside, b"test-only-outside-data\n").expect("write outside fixture");
    let canonical_root = source_root
        .canonicalize()
        .expect("canonicalize source root");

    assert!(validate_source_file(&allowed, &canonical_root).is_ok());
    assert!(validate_source_file(&outside, &canonical_root).is_err());
}

/// 绝对路径中出现父级跳转时直接拒绝，避免以规范化结果掩盖越界输入。
/// @author kongweiguang
#[test]
fn source_file_rejects_parent_traversal() {
    let temporary = TemporaryDirectory::new();
    let source_root = temporary.path().join("task-private-source");
    fs::create_dir(&source_root).expect("create source root");
    let outside = temporary.path().join("outside.fixture");
    fs::write(&outside, b"test-only-data\n").expect("write outside fixture");
    let traversing_path = source_root.join("..").join("outside.fixture");
    let canonical_root = source_root
        .canonicalize()
        .expect("canonicalize source root");

    assert!(validate_source_file(&traversing_path, &canonical_root).is_err());
}

/// 输出检查同时约束 target 范围和 run/文件不覆盖，保持 seeder 可安全重试。
/// @author kongweiguang
#[test]
fn output_must_be_new_and_inside_target() {
    let temporary = TemporaryDirectory::new();
    let target_root = temporary.path().join("target");
    let output_parent = target_root.join("acceptance");
    fs::create_dir_all(&output_parent).expect("create isolated target");
    let canonical_target = target_root
        .canonicalize()
        .expect("canonicalize target root");
    let new_run = output_parent.join("new-run");
    let nested_new_file = output_parent
        .join("nested-run")
        .join("config")
        .join("hosts")
        .join("host.toml");
    let existing_run = output_parent.join("existing-run");
    fs::create_dir(&existing_run).expect("create existing run");
    let outside = temporary.path().join("outside-run");

    assert!(validate_output_path(&new_run, &canonical_target).is_ok());
    assert!(validate_output_path(&nested_new_file, &canonical_target).is_ok());
    assert!(validate_output_path(&existing_run, &canonical_target).is_err());
    assert!(validate_output_path(&outside, &canonical_target).is_err());
}

/// 源与输出必须互不包含，以免复制过程覆盖任务私有 fixture。
/// @author kongweiguang
#[test]
fn source_and_output_roots_must_not_overlap() {
    let temporary = TemporaryDirectory::new();
    let source_root = temporary.path().join("source");
    let output_root = temporary.path().join("target/acceptance/output");
    let nested_root = source_root.join("child");

    assert!(!paths_overlap(&source_root, &output_root));
    assert!(paths_overlap(&source_root, &nested_root));
}

/// 不允许验收输出通过 target 内的符号链接落到另一个目录。
/// @author kongweiguang
#[cfg(unix)]
#[test]
fn output_rejects_symlink_parent() {
    use std::os::unix::fs::symlink;

    let temporary = TemporaryDirectory::new();
    let target_root = temporary.path().join("target");
    let output_parent = target_root.join("acceptance");
    let outside = temporary.path().join("outside");
    fs::create_dir_all(&output_parent).expect("create isolated target");
    fs::create_dir(&outside).expect("create outside directory");
    let alias = output_parent.join("alias");
    symlink(&outside, &alias).expect("create output symlink");
    let canonical_target = target_root
        .canonicalize()
        .expect("canonicalize target root");

    assert!(validate_output_path(&alias.join("new-run"), &canonical_target).is_err());
}

/// 符号链接即使最终仍指向源根内的文件也拒绝，避免路径别名带来竞态和歧义。
/// @author kongweiguang
#[cfg(unix)]
#[test]
fn source_file_rejects_symlink_alias() {
    use std::os::unix::fs::symlink;

    let temporary = TemporaryDirectory::new();
    let source_root = temporary.path().join("task-private-source");
    fs::create_dir(&source_root).expect("create source root");
    let original = source_root.join("original.fixture");
    fs::write(&original, b"test-only-data\n").expect("write fixture");
    let alias = source_root.join("alias.fixture");
    symlink(&original, &alias).expect("create test symlink");
    let canonical_root = source_root
        .canonicalize()
        .expect("canonicalize source root");

    assert!(validate_source_file(&alias, &canonical_root).is_err());
}
