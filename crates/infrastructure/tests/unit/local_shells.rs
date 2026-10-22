//! 本地 Shell 平台发现器的元数据与取消边界测试。
//!
//! @author kongweiguang

use std::{ffi::OsString, fs, path::PathBuf};

use kerminal_application::{PublicErrorCode, local_shells::LocalShellDiscovery};
use tokio_util::sync::CancellationToken;

use super::*;

/// 发现过程必须先响应取消，不读取 PATH 或返回看似成功的空列表。
#[test]
fn cancelled_discovery_returns_cancelled_error() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let error = SystemLocalShellDiscovery::new()
        .discover(&cancellation)
        .expect_err("取消后不得继续扫描");
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
}

/// 平台结果只包含有稳定 ID、绝对 executable 路径的有限候选，不触发任何候选进程。
#[test]
fn discovered_candidates_are_bounded_and_have_absolute_programs() {
    let candidates = SystemLocalShellDiscovery::new()
        .discover(&CancellationToken::new())
        .expect("文件系统发现应返回候选或空列表");

    assert!(candidates.len() <= MAX_LOCAL_SHELL_CANDIDATES);
    let mut ids = HashSet::new();
    for candidate in candidates {
        assert!(ids.insert(candidate.id().to_owned()));
        assert!(PathBuf::from(candidate.program()).is_absolute());
        assert!(
            candidate
                .display_name()
                .chars()
                .all(|character| !character.is_control())
        );
    }
}

/// Unix 权限位不满足时不能把普通文本文件当作 shell，Windows 则以实际存在的 exe 为界。
#[test]
fn executable_file_check_rejects_missing_paths() {
    let directory = tempfile::tempdir().expect("创建隔离测试目录");
    let missing = directory.path().join("missing-shell");
    assert!(!is_executable_file(&missing));

    let executable = directory
        .path()
        .join(if cfg!(windows) { "shell.exe" } else { "shell" });
    fs::write(&executable, b"fixture").expect("写入隔离测试文件");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert!(!is_executable_file(&executable));
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("设置可执行权限");
        assert!(is_executable_file(&executable));
    }
    #[cfg(windows)]
    assert!(is_executable_file(&executable));
}

/// 候选收集的内部辅助保持零化无关，确保候选 argv 可直接用 OsString 承载空格路径。
#[test]
fn shell_candidates_keep_arguments_as_separate_os_strings() {
    let directory = tempfile::tempdir().expect("创建隔离测试目录");
    let executable = directory
        .path()
        .join(if cfg!(windows) { "shell.exe" } else { "shell" });
    fs::write(&executable, b"fixture").expect("写入隔离测试文件");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("设置可执行权限");
    }
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let args = vec![OsString::from("argument with spaces")];

    push_candidate(
        &mut candidates,
        &mut seen,
        "test-shell",
        "Test Shell",
        executable,
        args.clone(),
        &CancellationToken::new(),
    )
    .expect("有效候选应被收集");

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].args(), args.as_slice());
}
