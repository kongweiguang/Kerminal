//! 本机路径来源的作用域、实时目录和 shell 追加转义测试。
//!
//! @author kongweiguang

use kerminal_application::commands::CommandSuggestionProvider;
use kerminal_domain::commands::{
    CommandHistoryTarget, CommandSuggestionRequest, SuggestionProviderKind, SuggestionQueryMode,
};
use kerminal_infrastructure::commands::suggestion_sources::LocalCommandSuggestionProvider;
use tokio_util::sync::CancellationToken;

/// 固定测试配置仅启用本地路径，避免测试依赖开发机器的 PATH。
fn request(input: &str, cwd: Option<String>, shell: &str) -> CommandSuggestionRequest {
    CommandSuggestionRequest {
        input: input.to_owned(),
        cursor: input.chars().count(),
        target: CommandHistoryTarget::Local,
        session_id: Some("session-test".to_owned()),
        pane_id: None,
        profile_id: None,
        remote_host_id: None,
        cwd,
        shell: Some(shell.to_owned()),
        providers: Some(vec![SuggestionProviderKind::LocalPath]),
        limit: Some(20),
        mode: SuggestionQueryMode::Inline,
        generation: Some(1),
        context_key: None,
    }
}

/// 空格目录沿用 PowerShell 原始前缀，只对新增部分按该 Shell 转义。
#[tokio::test]
async fn paths_use_confirmed_cwd_and_escape_only_the_suffix() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("folder with space")).unwrap();
    std::fs::create_dir(directory.path().join("Folder other")).unwrap();
    let query = request(
        "cd fol",
        Some(directory.path().to_string_lossy().into_owned()),
        "powershell",
    );
    let values = LocalCommandSuggestionProvider::new()
        .suggest(query, CancellationToken::new())
        .await
        .unwrap();
    assert!(values.iter().any(|value| {
        value.replacement_text == format!("folder` with` space{}", std::path::MAIN_SEPARATOR)
    }));
}

/// 只在目录实际接受不同大小写时补全，并保留已输入字符和 Unicode 字符边界。
#[cfg(windows)]
#[tokio::test]
async fn windows_path_case_matching_preserves_typed_prefix() {
    let directory = tempfile::tempdir().unwrap();
    for actual in ["Folder", "École"] {
        std::fs::create_dir(directory.path().join(actual)).unwrap();
    }
    let provider = LocalCommandSuggestionProvider::new();
    for (actual, prefix, completed) in [("Folder", "fol", "folder"), ("École", "éc", "école")] {
        let accepts_case_variant = directory.path().join(completed).is_dir();
        let query = request(
            &format!("cd {prefix}"),
            Some(directory.path().to_string_lossy().into_owned()),
            "powershell",
        );
        let values = provider
            .suggest(query, CancellationToken::new())
            .await
            .unwrap();
        let expected = format!("{completed}{}", std::path::MAIN_SEPARATOR);
        assert_eq!(
            values
                .iter()
                .any(|value| value.replacement_text == expected),
            accepts_case_variant,
            "actual: {actual:?}, prefix: {prefix:?}"
        );
    }
}

/// POSIX 单引号内的反斜杠属于文件名，父目录和新增目录分隔符都应保持原语义。
#[cfg(unix)]
#[tokio::test]
async fn unix_path_preserves_literal_backslash_in_basename() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("folder\\back")).unwrap();
    let query = request(
        "cd 'folder\\b",
        Some(directory.path().to_string_lossy().into_owned()),
        "bash",
    );
    let values = LocalCommandSuggestionProvider::new()
        .suggest(query, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        values
            .iter()
            .any(|value| value.replacement_text == "'folder\\back/'")
    );

    let parent = directory.path().join("other\\parent");
    std::fs::create_dir(&parent).unwrap();
    std::fs::create_dir(parent.join("child")).unwrap();
    let query = request(
        "cd 'other\\parent/ch",
        Some(directory.path().to_string_lossy().into_owned()),
        "bash",
    );
    let values = LocalCommandSuggestionProvider::new()
        .suggest(query, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        values
            .iter()
            .any(|value| value.replacement_text == "'other\\parent/child/'")
    );
}

/// 没有实时目录时不回退到应用进程目录，SSH 目标也不能使用本地文件系统。
#[tokio::test]
async fn unknown_cwd_and_remote_target_never_probe_local_relative_paths() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("folder")).unwrap();
    let provider = LocalCommandSuggestionProvider::new();
    let missing_cwd = request("cd fol", None, "powershell");
    assert!(
        provider
            .suggest(missing_cwd, CancellationToken::new())
            .await
            .unwrap()
            .is_empty()
    );
    let mut remote = request(
        "cd fol",
        Some(directory.path().to_string_lossy().into_owned()),
        "powershell",
    );
    remote.target = CommandHistoryTarget::Ssh;
    assert!(
        provider
            .suggest(remote, CancellationToken::new())
            .await
            .unwrap()
            .is_empty()
    );
}

/// Fish 双引号中的反引号不应被额外转义，结果可直接作为原 token 的追加。
#[tokio::test]
async fn fish_quoted_path_preserves_literal_backtick() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("foo`bar")).unwrap();
    let query = request(
        "cd \"fo",
        Some(directory.path().to_string_lossy().into_owned()),
        "fish",
    );
    let values = LocalCommandSuggestionProvider::new()
        .suggest(query, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].replacement_text, "\"foo`bar/\"");
}
