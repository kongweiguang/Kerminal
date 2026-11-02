//! 命令建议历史召回的会话复用、SSH 主机隔离和目录排序回归。
//!
//! @author kongweiguang

use std::{fs, path::PathBuf, sync::Arc};

use kerminal_application::commands::{
    CommandHistoryDraft, CommandHistoryRepository, CommandSuggestionProvider,
    HistoryCommandSuggestionProvider,
};
use kerminal_domain::{
    commands::{
        CommandHistorySource, CommandHistoryTarget, CommandSuggestionRequest,
        SuggestionProviderKind, SuggestionQueryMode,
    },
    identity::{HostId, PaneId, SessionId},
};
use kerminal_infrastructure::{
    command_history::SqliteCommandHistoryRepository, commands::FileCommandCatalogRepository,
    storage::StorageCoordinator,
};
use tokio_util::sync::CancellationToken;

struct TestRoot(PathBuf);

impl TestRoot {
    /// 使用独占随机目录，避免读取用户的历史库或与其他测试共享状态。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kerminal-suggestion-scope-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestRoot {
    /// 只清理当前用例创建的随机根，不触碰其他配置或历史。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 显式标记每条记录的来源会话和主机，避免测试偶然依赖真实环境。
fn draft(
    command: &str,
    target: CommandHistoryTarget,
    session: &str,
    host: Option<&str>,
    cwd: &str,
) -> CommandHistoryDraft {
    CommandHistoryDraft {
        command: command.to_owned(),
        source: CommandHistorySource::User,
        target,
        session_id: Some(SessionId::parse(session).unwrap()),
        pane_id: Some(PaneId::parse("pane-old").unwrap()),
        tab_id: None,
        profile_id: None,
        remote_host_id: host.map(|host| HostId::parse(host).unwrap()),
        cwd: Some(cwd.to_owned()),
        shell: Some("bash".to_owned()),
    }
}

/// 固定新会话和目录，以验证历史候选不会把旧会话作为硬性边界。
fn request(target: CommandHistoryTarget, host: Option<&str>) -> CommandSuggestionRequest {
    CommandSuggestionRequest {
        input: "git ".to_owned(),
        cursor: 4,
        target,
        session_id: Some("session-new".to_owned()),
        pane_id: Some("pane-new".to_owned()),
        profile_id: None,
        remote_host_id: host.map(str::to_owned),
        cwd: Some("/same".to_owned()),
        shell: Some("bash".to_owned()),
        providers: Some(vec![SuggestionProviderKind::History]),
        limit: Some(20),
        mode: SuggestionQueryMode::Inline,
        generation: Some(1),
        context_key: None,
    }
}

/// 使用真实 SQLite 筛选确认本地跨会话复用、SSH 同主机复用及缺少主机时拒绝查询。
#[tokio::test]
async fn history_reuses_sessions_without_crossing_target_or_ssh_host() {
    let root = TestRoot::new();
    let storage = StorageCoordinator::new(root.0.clone());
    let history = Arc::new(SqliteCommandHistoryRepository::open(storage.clone()).unwrap());
    for entry in [
        draft(
            "git local-old",
            CommandHistoryTarget::Local,
            "session-old",
            None,
            "/same",
        ),
        draft(
            "git local-new",
            CommandHistoryTarget::Local,
            "session-new",
            None,
            "/other",
        ),
        draft(
            "git remote-old",
            CommandHistoryTarget::Ssh,
            "session-old",
            Some("host-a"),
            "/same",
        ),
        draft(
            "git remote-new",
            CommandHistoryTarget::Ssh,
            "session-new",
            Some("host-a"),
            "/other",
        ),
        draft(
            "git foreign",
            CommandHistoryTarget::Ssh,
            "session-new",
            Some("host-b"),
            "/same",
        ),
    ] {
        history.append(entry).await.unwrap();
    }
    let provider = HistoryCommandSuggestionProvider::new(
        history,
        Arc::new(FileCommandCatalogRepository::new(storage, true)),
    );

    let local = provider
        .suggest(
            request(CommandHistoryTarget::Local, None),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(local.len(), 2);
    assert_eq!(local[0].replacement_text, "git local-old");
    assert!(
        local
            .iter()
            .any(|item| item.replacement_text == "git local-new")
    );

    let remote = provider
        .suggest(
            request(CommandHistoryTarget::Ssh, Some("host-a")),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(remote.len(), 2);
    assert_eq!(remote[0].replacement_text, "git remote-old");
    assert!(
        remote
            .iter()
            .any(|item| item.replacement_text == "git remote-new")
    );
    assert!(
        !remote
            .iter()
            .any(|item| item.replacement_text == "git foreign")
    );

    let missing_host = provider
        .suggest(
            request(CommandHistoryTarget::Ssh, None),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(missing_host.is_empty());
}

/// 历史召回应先筛选真实前缀，再限制数量，避免较新的包含匹配挤掉有效候选。
#[tokio::test]
async fn history_prefix_filter_precedes_recall_limit() {
    let root = TestRoot::new();
    let storage = StorageCoordinator::new(root.0.clone());
    let history = Arc::new(SqliteCommandHistoryRepository::open(storage.clone()).unwrap());
    history
        .append(draft(
            "git status",
            CommandHistoryTarget::Local,
            "session-old",
            None,
            "/same",
        ))
        .await
        .unwrap();
    for index in 0..101 {
        history
            .append(draft(
                &format!("echo git {index}"),
                CommandHistoryTarget::Local,
                "session-new",
                None,
                "/same",
            ))
            .await
            .unwrap();
    }
    let provider = HistoryCommandSuggestionProvider::new(
        history,
        Arc::new(FileCommandCatalogRepository::new(storage, true)),
    );

    let candidates = provider
        .suggest(
            request(CommandHistoryTarget::Local, None),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].replacement_text, "git status");
    assert_eq!(candidates[0].suffix, "status");
}

/// Unicode 前缀必须逐字符精确匹配，避免数据库折叠规则漏召回或改写原有输入。
#[tokio::test]
async fn history_recall_preserves_exact_unicode_prefixes() {
    let root = TestRoot::new();
    let storage = StorageCoordinator::new(root.0.clone());
    let history = Arc::new(SqliteCommandHistoryRepository::open(storage.clone()).unwrap());
    for command in [
        "Éclair",
        "éclair",
        "e\u{301}clair",
        "eclair",
        "\u{1d11e} tool",
        "printf_%done",
        "printf_Xdone",
    ] {
        history
            .append(draft(
                command,
                CommandHistoryTarget::Local,
                "session-old",
                None,
                "/same",
            ))
            .await
            .unwrap();
    }
    let provider = HistoryCommandSuggestionProvider::new(
        history,
        Arc::new(FileCommandCatalogRepository::new(storage, true)),
    );

    for (prefix, expected) in [
        ("Éc", "Éclair"),
        ("e\u{301}c", "e\u{301}clair"),
        ("\u{1d11e} ", "\u{1d11e} tool"),
        ("printf_%", "printf_%done"),
    ] {
        let mut query = request(CommandHistoryTarget::Local, None);
        query.input = prefix.to_owned();
        query.cursor = prefix.chars().count();
        let candidates = provider
            .suggest(query, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(candidates.len(), 1, "prefix: {prefix:?}");
        assert_eq!(candidates[0].replacement_text, expected);
        assert_eq!(candidates[0].suffix, &expected[prefix.len()..]);
    }
}
