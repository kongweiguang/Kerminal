//! 配置目录 opener 的路径包含关系与参数边界测试。
//!
//! @author kongweiguang

use std::{fs, path::PathBuf};

use kerminal_application::ConfigurationDirectoryKind;
use tempfile::tempdir;

use super::{opener_invocation, resolve_configuration_directory};

/// 恢复入口只解析既有配置根，不创建子目录或改写损坏配置。
///
/// @author kongweiguang
#[test]
fn resolves_the_existing_configuration_root_without_writing_children() {
    let root = tempdir().expect("temporary config root");
    let canonical_root = root.path().canonicalize().expect("canonical config root");

    let directory = resolve_configuration_directory(root.path(), ConfigurationDirectoryKind::Root)
        .expect("configuration root");

    assert_eq!(directory, canonical_root);
    assert!(directory.is_dir());
    assert_eq!(
        fs::read_dir(&directory).expect("read root entries").count(),
        0
    );
}

/// 配置根在请求前已删除时拒绝打开，保持恢复入口只读且绝不重建数据目录。
///
/// @author kongweiguang
#[test]
fn refuses_to_create_a_missing_configuration_root() {
    let parent = tempdir().expect("temporary parent");
    let missing_root = parent.path().join("removed-config-root");

    assert!(
        resolve_configuration_directory(&missing_root, ConfigurationDirectoryKind::Root).is_err()
    );
    assert!(!missing_root.exists());
}

/// 配置根被替换为普通文件时明确拒绝，避免把文件路径交给系统文件管理器。
///
/// @author kongweiguang
#[test]
fn rejects_a_regular_file_as_configuration_root() {
    let parent = tempdir().expect("temporary parent");
    let file_root = parent.path().join("config-root-file");
    fs::write(&file_root, b"preserve").expect("write sentinel file");

    assert!(resolve_configuration_directory(&file_root, ConfigurationDirectoryKind::Root).is_err());
    assert_eq!(
        fs::read(&file_root).expect("read sentinel file"),
        b"preserve"
    );
}

/// 首次打开片段目录时只创建配置根下的固定子目录。
#[test]
fn creates_only_the_snippets_child_under_config_root() {
    let root = tempdir().expect("temporary config root");
    let directory =
        resolve_configuration_directory(root.path(), ConfigurationDirectoryKind::Snippets)
            .expect("snippets directory");

    assert_eq!(
        directory,
        root.path().canonicalize().unwrap().join("snippets")
    );
    assert!(directory.is_dir());
}

/// 同名配置路径若是文件则拒绝打开，不会覆盖或转换用户数据。
#[test]
fn rejects_a_regular_file_at_the_snippets_directory_path() {
    let root = tempdir().expect("temporary config root");
    fs::write(root.path().join("snippets"), b"preserve").expect("write sentinel file");

    assert!(
        resolve_configuration_directory(root.path(), ConfigurationDirectoryKind::Snippets).is_err()
    );
    assert_eq!(
        fs::read(root.path().join("snippets")).expect("read sentinel file"),
        b"preserve"
    );
}

/// 配置路径通过链接逃出数据根时拒绝请求。
#[cfg(unix)]
#[test]
fn rejects_a_snippets_symlink_that_escapes_config_root() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("temporary config root");
    let outside = tempdir().expect("temporary outside root");
    symlink(outside.path(), root.path().join("snippets")).expect("create symlink");

    assert!(
        resolve_configuration_directory(root.path(), ConfigurationDirectoryKind::Snippets).is_err()
    );
}

/// 即使符号链接仍指向配置根内部，也不能把“片段目录”请求重定向到其它配置类型。
#[cfg(unix)]
#[test]
fn rejects_a_snippets_symlink_to_another_in_root_directory() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("temporary config root");
    fs::create_dir(root.path().join("hosts")).expect("create hosts directory");
    symlink(root.path().join("hosts"), root.path().join("snippets")).expect("create symlink");

    assert!(
        resolve_configuration_directory(root.path(), ConfigurationDirectoryKind::Snippets).is_err()
    );
}

/// 路径作为一个 OS 参数传递，空格、引号和分号不经过 shell 解释。
#[test]
fn opener_receives_the_directory_as_one_unmodified_argument() {
    let path = PathBuf::from(r#"C:\config root\snippets;"#);
    let invocation = opener_invocation(&path).expect("supported OS opener");

    assert_eq!(invocation.arguments.len(), 1);
    assert_eq!(invocation.arguments[0], path.as_os_str());
}
