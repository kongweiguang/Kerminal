//! Windows 原子提交的 staging 耐久化与只读冲突回归。
//!
//! @author kongweiguang

#![cfg(windows)]

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use kerminal_infrastructure::storage::{AtomicCommitError, CommitMode, commit_staged_file};

/// 在测试专属目录写入并关闭 staging writer，模拟下载完成后的提交输入。
fn write_closed_stage(directory: &Path, name: &str, contents: &[u8]) -> PathBuf {
    let path = directory.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .expect("create isolated staging file");
    file.write_all(contents)
        .expect("write isolated staging contents");
    drop(file);
    path
}

/// 只在隔离目录测试文件上切换 Windows 的只读属性，清理时保证 TempDir 可删除。
fn set_readonly(path: &Path, readonly: bool) -> std::io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(readonly);
    fs::set_permissions(path, permissions)
}

/// 写完并关闭 staging 文件后，CreateNew 仍必须用可 flush 的句柄同步并原子落位。
#[test]
fn closed_stage_is_synced_before_create_new_commit() {
    let root = tempfile::tempdir().expect("create isolated commit root");
    let source = write_closed_stage(root.path(), "new.pending", b"new content");
    let destination = root.path().join("new.txt");

    let result = commit_staged_file(&source, &destination, CommitMode::CreateNew);
    let contents = fs::read(&destination).ok();
    let stage_remains = source.exists();

    assert!(result.is_ok(), "closed stage should commit: {result:?}");
    assert_eq!(contents.as_deref(), Some(&b"new content"[..]));
    assert!(
        !stage_remains,
        "successful rename consumes the staging name"
    );
}

/// 替换模式也要先完成 staging flush，不能把 closed writer 的内容误报为权限失败。
#[test]
fn closed_stage_is_synced_before_replace_existing_commit() {
    let root = tempfile::tempdir().expect("create isolated commit root");
    let source = write_closed_stage(root.path(), "replace.pending", b"replacement");
    let destination = root.path().join("existing.txt");
    fs::write(&destination, b"old content").expect("write existing destination");

    let result = commit_staged_file(&source, &destination, CommitMode::ReplaceExisting);
    let contents = fs::read(&destination).ok();
    let stage_remains = source.exists();

    assert!(
        result.is_ok(),
        "closed stage should replace destination: {result:?}"
    );
    assert_eq!(contents.as_deref(), Some(&b"replacement"[..]));
    assert!(
        !stage_remains,
        "successful replacement consumes the staging name"
    );
}

/// 远端源权限可令 staging 只读；flush 时临时调整的只能是 staging，提交后仍保留只读语义。
#[test]
fn readonly_stage_can_be_created_and_remains_readonly() {
    let root = tempfile::tempdir().expect("create isolated commit root");
    let source = write_closed_stage(root.path(), "readonly.pending", b"readonly content");
    set_readonly(&source, true).expect("mark only the staged file readonly");
    let destination = root.path().join("readonly.txt");

    let result = commit_staged_file(&source, &destination, CommitMode::CreateNew);
    let committed_contents = fs::read(&destination).ok();
    let committed_readonly = fs::metadata(&destination)
        .map(|metadata| metadata.permissions().readonly())
        .unwrap_or(false);
    let stage_remains = source.exists();
    if destination.exists() {
        set_readonly(&destination, false).expect("allow isolated temp cleanup");
    }
    if stage_remains {
        set_readonly(&source, false).expect("allow isolated temp cleanup");
    }

    assert!(
        result.is_ok(),
        "readonly stage should be flushed and committed: {result:?}"
    );
    assert_eq!(
        committed_contents.as_deref(),
        Some(&b"readonly content"[..])
    );
    assert!(
        committed_readonly,
        "CreateNew preserves staged readonly mode"
    );
    assert!(!stage_remains);
}

/// 已存在的只读目标保持原内容；no-clobber 冲突同时保留 staging 及其原权限。
#[test]
fn readonly_stage_conflict_keeps_both_files_and_reports_already_exists() {
    let root = tempfile::tempdir().expect("create isolated commit root");
    let source = write_closed_stage(root.path(), "conflict.pending", b"new content");
    let destination = root.path().join("existing-readonly.txt");
    fs::write(&destination, b"existing content").expect("write existing destination");
    set_readonly(&source, true).expect("mark staging readonly");
    set_readonly(&destination, true).expect("mark destination readonly");

    let error = commit_staged_file(&source, &destination, CommitMode::CreateNew)
        .expect_err("CreateNew must not replace an existing readonly destination");
    let (kind, source_exists, source_readonly, destination_contents, destination_readonly) =
        match error {
            AtomicCommitError::NotCommitted(error) => (
                error.kind(),
                source.exists(),
                fs::metadata(&source)
                    .map(|metadata| metadata.permissions().readonly())
                    .unwrap_or(false),
                fs::read(&destination).ok(),
                fs::metadata(&destination)
                    .map(|metadata| metadata.permissions().readonly())
                    .unwrap_or(false),
            ),
            AtomicCommitError::CommitUnknown(_) => (
                std::io::ErrorKind::Other,
                source.exists(),
                false,
                fs::read(&destination).ok(),
                false,
            ),
        };
    let source_cleanup = if source.exists() {
        set_readonly(&source, false)
    } else {
        Ok(())
    };
    let destination_cleanup = set_readonly(&destination, false);

    assert_eq!(kind, std::io::ErrorKind::AlreadyExists);
    assert!(source_exists, "failed no-clobber move retains its stage");
    assert!(source_readonly, "failed commit restores staged permissions");
    assert_eq!(
        destination_contents.as_deref(),
        Some(&b"existing content"[..])
    );
    assert!(
        destination_readonly,
        "conflict leaves destination permissions unchanged"
    );
    assert!(source_cleanup.is_ok());
    assert!(destination_cleanup.is_ok());
}

/// 既有只读用户文件仅改变目录项；纯 rename 不打开文件，也不触碰原始只读属性。
#[test]
fn readonly_existing_file_move_preserves_attributes() {
    let root = tempfile::tempdir().expect("create isolated commit root");
    let source = root.path().join("user-readonly.txt");
    fs::write(&source, b"user content").expect("write isolated user file");
    set_readonly(&source, true).expect("mark only the isolated user file readonly");
    let destination = root.path().join("moved-readonly.txt");

    let result = commit_staged_file(&source, &destination, CommitMode::MoveNoReplace);
    let contents = fs::read(&destination).ok();
    let destination_readonly = fs::metadata(&destination)
        .map(|metadata| metadata.permissions().readonly())
        .unwrap_or(false);

    assert!(result.is_ok(), "readonly source should move: {result:?}");
    assert!(
        !source.exists(),
        "successful rename consumes the source name"
    );
    assert_eq!(contents.as_deref(), Some(&b"user content"[..]));
    assert!(
        destination_readonly,
        "rename preserves the source readonly bit"
    );
}

/// 用户只读源遇到 no-clobber 冲突时原样保留，既有目标内容与属性也不变。
#[test]
fn readonly_existing_file_move_conflict_preserves_both_files() {
    let root = tempfile::tempdir().expect("create isolated commit root");
    let source = root.path().join("user-readonly.txt");
    let destination = root.path().join("existing-readonly.txt");
    fs::write(&source, b"user content").expect("write isolated user file");
    fs::write(&destination, b"existing content").expect("write isolated destination");
    set_readonly(&source, true).expect("mark only the isolated user file readonly");
    set_readonly(&destination, true).expect("mark only the isolated destination readonly");

    let error = commit_staged_file(&source, &destination, CommitMode::MoveNoReplace)
        .expect_err("no-clobber rename must not replace an existing file");
    let (kind, source_readonly, destination_contents, destination_readonly) = match error {
        AtomicCommitError::NotCommitted(error) => (
            error.kind(),
            fs::metadata(&source)
                .map(|metadata| metadata.permissions().readonly())
                .unwrap_or(false),
            fs::read(&destination).ok(),
            fs::metadata(&destination)
                .map(|metadata| metadata.permissions().readonly())
                .unwrap_or(false),
        ),
        AtomicCommitError::CommitUnknown(_) => (
            std::io::ErrorKind::Other,
            false,
            fs::read(&destination).ok(),
            false,
        ),
    };
    let source_exists = source.exists();
    let source_cleanup = set_readonly(&source, false);
    let destination_cleanup = set_readonly(&destination, false);

    assert_eq!(kind, std::io::ErrorKind::AlreadyExists);
    assert!(source_exists, "failed rename retains the user source");
    assert!(source_readonly, "failed rename preserves source attributes");
    assert_eq!(
        destination_contents.as_deref(),
        Some(&b"existing content"[..])
    );
    assert!(
        destination_readonly,
        "failed rename preserves destination attributes"
    );
    assert!(source_cleanup.is_ok());
    assert!(destination_cleanup.is_ok());
}

/// 内容 staging 需先 flush；跨同卷目录 no-clobber 移动显式使用 staging 模式。
#[test]
fn staged_file_move_no_replace_syncs_across_parent_directories() {
    let root = tempfile::tempdir().expect("create isolated commit root");
    let staging_directory = root.path().join("staging");
    let destination_directory = root.path().join("destination");
    fs::create_dir(&staging_directory).expect("create staging directory");
    fs::create_dir(&destination_directory).expect("create destination directory");
    let source = write_closed_stage(&staging_directory, "content.pending", b"staged content");
    set_readonly(&source, true).expect("mark only the staged file readonly");
    let destination = destination_directory.join("content.txt");

    let result = commit_staged_file(&source, &destination, CommitMode::MoveStagedNoReplace);
    let contents = fs::read(&destination).ok();
    let destination_readonly = fs::metadata(&destination)
        .map(|metadata| metadata.permissions().readonly())
        .unwrap_or(false);
    if destination.exists() {
        set_readonly(&destination, false).expect("allow isolated temp cleanup");
    }
    if source.exists() {
        set_readonly(&source, false).expect("allow isolated temp cleanup");
    }

    assert!(result.is_ok(), "staged file should move: {result:?}");
    assert!(
        !source.exists(),
        "successful rename consumes the staging name"
    );
    assert_eq!(contents.as_deref(), Some(&b"staged content"[..]));
    assert!(
        destination_readonly,
        "staging mode preserves readonly permissions"
    );
}

/// 构造超过传统 MAX_PATH 的隔离目录，令测试覆盖直接调用 Win32 提交 API 的真实边界。
///
/// @author kongweiguang
fn deep_test_directory(root: &Path) -> PathBuf {
    use std::os::windows::ffi::OsStrExt;

    let mut directory = root.to_path_buf();
    while directory
        .join("stage.pending")
        .as_os_str()
        .encode_wide()
        .count()
        <= 300
    {
        directory.push("deep-path-segment-0123456789abcdef");
    }
    fs::create_dir_all(&directory).expect("create only the nested temporary directories");
    directory
}

/// 两种文件提交都应使用能表达深路径的 Win32 名称，且不弱化原子 no-clobber/替换语义。
///
/// @author kongweiguang
#[test]
fn direct_win32_commit_helpers_support_paths_over_legacy_max_path() {
    use std::os::windows::ffi::OsStrExt;

    let root = tempfile::tempdir().expect("create isolated commit root");
    let directory = deep_test_directory(root.path());

    let create_source = write_closed_stage(&directory, "create.pending", b"create payload");
    let create_destination = directory.join("created.txt");
    assert!(
        create_destination.as_os_str().encode_wide().count() > 260,
        "CreateNew destination exceeds legacy MAX_PATH"
    );
    let create_result =
        commit_staged_file(&create_source, &create_destination, CommitMode::CreateNew);
    assert!(
        create_result.is_ok(),
        "CreateNew must commit an extended path without replacing an existing name: {create_result:?}"
    );
    assert_eq!(fs::read(&create_destination).unwrap(), b"create payload");
    assert!(!create_source.exists());

    let conflict_source = write_closed_stage(&directory, "conflict.pending", b"must remain staged");
    let conflict = commit_staged_file(&conflict_source, &create_destination, CommitMode::CreateNew)
        .expect_err("CreateNew must retain no-clobber semantics on an extended path");
    assert!(matches!(
        conflict,
        AtomicCommitError::NotCommitted(ref error)
            if error.kind() == std::io::ErrorKind::AlreadyExists
    ));
    assert!(conflict_source.exists());
    assert_eq!(fs::read(&create_destination).unwrap(), b"create payload");

    let replace_source = write_closed_stage(&directory, "replace.pending", b"replacement payload");
    let replace_result = commit_staged_file(
        &replace_source,
        &create_destination,
        CommitMode::ReplaceExisting,
    );
    assert!(
        replace_result.is_ok(),
        "ReplaceExisting must retain atomic replacement semantics on an extended path: {replace_result:?}"
    );
    assert_eq!(
        fs::read(&create_destination).unwrap(),
        b"replacement payload"
    );
    assert!(!replace_source.exists());
}
