//! 本地候选路径边界与真实 Git 引用回归。
//!
//! @author kongweiguang

use std::{path::Path, process::Command};

use kerminal_application::AppError;
use kerminal_domain::commands::{
    CommandHistoryTarget, CommandSuggestionRequest, SuggestionProviderKind, SuggestionQueryMode,
};
use tokio_util::sync::CancellationToken;

use super::git_suggestions;
#[cfg(windows)]
use super::{is_network_or_device_path, resolve_directory};
#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Git 夹具固定本地配置和禁用交互，避免执行用户的钩子或读取用户仓库设置。
fn run_git(cwd: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", cwd.join(".missing-global-config"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_TEMPLATE_DIR")
        .stdin(std::process::Stdio::null());
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let output = command
        .output()
        .expect("Git is required for the local ref test");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// 每个仓库只创建一个空提交；显式禁用提交钩子和签名以隔离机器级行为。
fn initialize_repository(cwd: &Path) {
    std::fs::create_dir_all(cwd).expect("create isolated Git repository");
    run_git(cwd, &["init", "--quiet"]);
    run_git(
        cwd,
        &[
            "-c",
            "user.name=Kerminal Test",
            "-c",
            "user.email=kerminal@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=.kerminal-no-hooks",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    );
}

/// 只请求 Git 来源，使回归断言与本机 PATH 和历史数据无关。
fn git_request(cwd: Option<&Path>) -> CommandSuggestionRequest {
    let input = "git checkout feature-";
    CommandSuggestionRequest {
        input: input.to_owned(),
        cursor: input.chars().count(),
        target: CommandHistoryTarget::Local,
        session_id: Some("session-test".to_owned()),
        pane_id: None,
        profile_id: None,
        remote_host_id: None,
        cwd: cwd.map(|path| path.to_string_lossy().into_owned()),
        shell: Some("powershell".to_owned()),
        providers: Some(vec![SuggestionProviderKind::Git]),
        limit: Some(20),
        mode: SuggestionQueryMode::Inline,
        generation: Some(1),
        context_key: None,
    }
}

/// 同时检查显示值和追加值，确保动态 refs 真正进入可用候选。
async fn suggested_refs(cwd: &Path) -> Vec<String> {
    git_suggestions(&git_request(Some(cwd)), CancellationToken::new())
        .await
        .expect("read local Git refs")
        .into_iter()
        .map(|candidate| {
            assert_eq!(candidate.replacement_text, candidate.display_text);
            candidate.display_text
        })
        .collect()
}

/// 在解析层拒绝 UNC 和设备路径；测试只构造路径值，不触碰网络或设备。
#[cfg(windows)]
#[test]
fn windows_network_and_device_prefixes_are_rejected_without_io() {
    for path in [
        r"\\server\share\folder\",
        "//server/share/folder/",
        r"/\server\share\folder\",
        r"\/server/share/folder/",
        r"\\?\UNC\server\share\folder\",
        r"\\.\pipe\example\",
        "//./pipe/example/",
        r"\\?\C:\folder\",
    ] {
        assert!(is_network_or_device_path(Path::new(path)), "{path}");
        assert_eq!(resolve_directory(path, Some(r"C:\local")), None, "{path}");
    }
    for path in [r"C:\local\folder\", "C:/local/folder/"] {
        assert!(!is_network_or_device_path(Path::new(path)), "{path}");
        assert_eq!(
            resolve_directory(path, None).as_deref(),
            Some(Path::new(path)),
            "{path}"
        );
    }
}

/// 连续查询必须反映当前仓库的分支、标签和目录变化，不能复用过期结果。
#[tokio::test]
async fn local_git_refs_follow_repository_changes_and_current_directory() {
    let fixture = tempfile::tempdir().expect("create isolated Git fixture");
    let first = fixture.path().join("first");
    let second = fixture.path().join("second");
    initialize_repository(&first);
    initialize_repository(&second);
    run_git(&first, &["branch", "feature-initial"]);
    run_git(&first, &["tag", "feature-tag-initial"]);
    run_git(&second, &["branch", "feature-second"]);
    run_git(&second, &["tag", "feature-tag-second"]);

    assert_eq!(
        suggested_refs(&first).await,
        ["feature-initial", "feature-tag-initial"]
    );
    assert_eq!(
        suggested_refs(&second).await,
        ["feature-second", "feature-tag-second"]
    );

    run_git(&first, &["branch", "-D", "feature-initial"]);
    run_git(&first, &["tag", "-d", "feature-tag-initial"]);
    run_git(&first, &["branch", "feature-updated"]);
    run_git(&first, &["tag", "feature-tag-updated"]);
    assert_eq!(
        suggested_refs(&first).await,
        ["feature-tag-updated", "feature-updated"]
    );
    assert_eq!(
        suggested_refs(&second).await,
        ["feature-second", "feature-tag-second"]
    );
}

/// 预先取消必须优先于缺少工作目录的降级，不应启动任何 Git 进程。
#[tokio::test]
async fn cancelled_local_git_request_stops_before_repository_lookup() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        git_suggestions(&git_request(None), cancellation).await,
        Err(AppError::Cancelled { .. })
    ));
}
