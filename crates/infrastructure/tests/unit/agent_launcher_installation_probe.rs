//! PATH 探测只确认固定 CLI 文件存在，且整个过程不会执行命令。
//!
//! @author kongweiguang

use std::ffi::OsStr;

use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

use super::executable_on_path;

/// 覆盖 Unix 命令名和 Windows PATHEXT 分支，避免 UI 把目录项误报为安装状态。
#[test]
fn checks_path_file_names_without_running_the_command() {
    let directory = tempdir().expect("temporary PATH directory");
    #[cfg(windows)]
    let executable_name = "codex.CMD";
    #[cfg(not(windows))]
    let executable_name = "codex";
    std::fs::write(directory.path().join(executable_name), b"placeholder")
        .expect("write fixture CLI marker");

    let path_ext = if cfg!(windows) {
        ".COM;.EXE;.BAT;.CMD"
    } else {
        ""
    };
    let cancellation = CancellationToken::new();
    assert!(
        executable_on_path(
            "codex",
            Some(directory.path().as_os_str()),
            Some(OsStr::new(path_ext)),
            &cancellation,
        )
        .expect("PATH probe succeeds")
    );
    assert!(
        !executable_on_path(
            "missing-agent",
            Some(directory.path().as_os_str()),
            Some(OsStr::new(path_ext)),
            &cancellation,
        )
        .expect("missing commands are a normal false result")
    );
}

/// 取消信号必须在访问 PATH 目录之前收敛，不让已失效刷新提交过期状态。
#[test]
fn honors_cancellation_before_scanning_path() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(
        executable_on_path("codex", Some(OsStr::new("/usr/bin")), None, &cancellation,).is_err()
    );
}
