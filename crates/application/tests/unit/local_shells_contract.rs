//! 本地 Shell 候选应用契约测试。
//!
//! @author kongweiguang

use std::{ffi::OsString, path::PathBuf};

use super::*;

/// 候选只能携带稳定身份和绝对程序路径，防止 UI 意外接受相对工作目录可变的命令。
/// @author kongweiguang
#[test]
fn local_shell_candidate_requires_stable_id_and_absolute_program() {
    let candidate = LocalShellCandidate::new(
        "pwsh",
        "PowerShell 7",
        std::env::current_exe()
            .expect("测试可执行文件路径")
            .into_os_string(),
        vec![OsString::from("-NoLogo")],
    )
    .expect("有效候选应被接受");

    assert_eq!(candidate.id(), "pwsh");
    assert_eq!(candidate.display_name(), "PowerShell 7");
    assert!(PathBuf::from(candidate.program()).is_absolute());

    assert!(
        LocalShellCandidate::new(
            "invalid id",
            "Shell",
            std::env::current_exe()
                .expect("测试可执行文件路径")
                .into_os_string(),
            Vec::new(),
        )
        .is_err()
    );
    assert!(
        LocalShellCandidate::new(
            "relative",
            "Shell",
            PathBuf::from("shell").into_os_string(),
            Vec::new(),
        )
        .is_err()
    );
}
