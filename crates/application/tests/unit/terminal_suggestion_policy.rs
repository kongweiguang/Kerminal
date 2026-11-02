//! Terminal inline suggestion preview policy regression coverage.
//!
//! @author kongweiguang

use super::*;
use crate::commands::SuggestionSettingsPreview;
use kerminal_domain::{
    commands::{CommandSuggestionReplacementRange, CommandSuggestionSensitivity},
    connections::RemoteShellDialect,
    identity::{PaneId, SessionId, TabId},
};

/// 使用固定 UI binding 构造查询，避免把候选规则测试与真实终端生命周期耦合。
fn query(input: &str, mode: SuggestionQueryMode) -> TerminalSuggestionQuery {
    TerminalSuggestionQuery {
        binding: UiSessionBinding {
            session_id: SessionId::parse("session-test").unwrap(),
            owner: SessionOwner::Ui {
                tab_id: TabId::parse("tab-main").unwrap(),
                pane_id: PaneId::parse("pane-main").unwrap(),
            },
            session_generation: 3,
        },
        expected_context: TerminalCommandContextVersion {
            session_generation: 3,
            prompt_epoch: 12,
            input_generation: 4,
        },
        request_generation: 8,
        input: input.to_owned(),
        cursor: input.chars().count(),
        mode,
    }
}

/// 不信任 provider 给出的后缀，测试用错误值确认终端根据替换文本独立计算。
fn candidate(id: &str, replacement: &str, start: usize, end: usize) -> CommandSuggestionCandidate {
    CommandSuggestionCandidate {
        id: id.to_owned(),
        provider: SuggestionProviderKind::Spec,
        activation: CommandSuggestionActivation::Insert,
        display_text: replacement.to_owned(),
        replacement_text: replacement.to_owned(),
        replacement_range: CommandSuggestionReplacementRange { start, end },
        suffix: "untrusted".to_owned(),
        score: 1.0,
        sensitivity: CommandSuggestionSensitivity::Normal,
        description: None,
        source_explanation: Some(id.to_owned()),
        merged_source_explanations: Vec::new(),
        source_id: None,
        allowed_presentations: CommandSuggestionCandidate::allowed_for(
            CommandSuggestionSensitivity::Normal,
        ),
        accept_boundaries: Vec::new(),
        context_key: None,
    }
}

/// 两种范围产生同一后缀时只展示一次，错误大小写和控制字符不得变成终端输入。
#[test]
fn terminal_only_retains_unique_strict_appendable_candidates() {
    let query = query("git che", SuggestionQueryMode::Inline);
    let values = terminal_candidates(
        &query,
        vec![
            candidate("token", "checkout", 4, 7),
            candidate("line", "git checkout", 0, 7),
            candidate("wrong-case", "Checkout", 4, 7),
            candidate("wrong-end", "checkout", 4, 6),
            candidate("empty", "che", 4, 7),
            candidate("control", "che\n", 4, 7),
        ],
    );
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].id, "token");
    assert_eq!(values[0].suffix, "ckout");
    assert_eq!(values[0].merged_source_explanations, ["line"]);
}

/// 可见候选必须能通过真实 writer 的字节与整行上限，避免用户接受后才发现确定无法追加。
#[test]
fn terminal_candidates_obey_shell_append_size_limits() {
    for (input, suffix, expected) in [
        ("echo ".to_owned(), "😀".repeat(1_024), true),
        ("echo ".to_owned(), "😀".repeat(1_025), false),
        ("x".repeat(3_999), "a".to_owned(), true),
        ("x".repeat(3_999), "ab".to_owned(), false),
    ] {
        let query = query(&input, SuggestionQueryMode::Menu);
        let values = terminal_candidates(
            &query,
            vec![candidate("bounded", &suffix, query.cursor, query.cursor)],
        );
        assert_eq!(
            !values.is_empty(),
            expected,
            "input chars={}, suffix bytes={}",
            input.chars().count(),
            suffix.len()
        );
    }
}

/// 行中光标不接收插入候选，而片段动作只在显式片段菜单内保留。
#[test]
fn terminal_rejects_middle_of_line_and_preserves_explicit_snippets() {
    let mut middle = query("git che", SuggestionQueryMode::Menu);
    middle.cursor = 4;
    assert!(terminal_candidates(&middle, vec![candidate("line", "git status", 0, 4)]).is_empty());

    let mut snippet = candidate("snippet", "snippet-name", 0, 2);
    snippet.activation = CommandSuggestionActivation::OpenSnippetPanel;
    let menu = query("::", SuggestionQueryMode::Menu);
    assert_eq!(terminal_candidates(&menu, vec![snippet.clone()]).len(), 1);
    let inline = query("::", SuggestionQueryMode::Inline);
    assert!(terminal_candidates(&inline, vec![snippet.clone()]).is_empty());
    let unrelated = query("git", SuggestionQueryMode::Menu);
    assert!(terminal_candidates(&unrelated, vec![snippet]).is_empty());
}

/// 避免旧 prompt、旧输入和未知投影被误当成当前可信上下文。
#[test]
fn terminal_context_rejects_stale_versions_and_unavailable_projection() {
    use crate::sessions::{TerminalCommandContext, TerminalCommandUnavailableReason};

    let query = query("git", SuggestionQueryMode::Inline);
    let mut runtime = TerminalCommandContext {
        session_id: query.binding.session_id.clone(),
        owner: query.binding.owner.clone(),
        session_generation: query.expected_context.session_generation,
        prompt_epoch: query.expected_context.prompt_epoch,
        input_generation: query.expected_context.input_generation,
        shell_dialect: RemoteShellDialect::Posix,
        prompt_ready: true,
        prompt_line_empty: false,
        alternate_screen: false,
        availability: TerminalCommandAvailability::Available,
    };
    assert!(context_matches(&query, &runtime));
    runtime.input_generation += 1;
    assert!(!context_matches(&query, &runtime));
    runtime.input_generation -= 1;
    runtime.prompt_epoch += 1;
    assert!(!context_matches(&query, &runtime));
    runtime.prompt_epoch -= 1;
    runtime.session_generation += 1;
    assert!(!context_matches(&query, &runtime));
    runtime.session_generation -= 1;
    runtime.availability = TerminalCommandAvailability::Unavailable(
        TerminalCommandUnavailableReason::UiInputProjectionInvalidated,
    );
    assert!(!context_matches(&query, &runtime));
}

/// 预览快照更新后下一次来源选择立即采用最新设置，不依赖磁盘保存或 watcher 延迟。
#[test]
fn terminal_suggestion_provider_selection_uses_latest_preview() {
    let preview = SuggestionSettingsPreview::default();
    let mut first = TerminalInlineSuggestionSettings::default();
    first.providers.history = false;
    first.providers.spec = false;
    preview.update(&first);

    let mut latest = TerminalInlineSuggestionSettings::default();
    latest.providers.history = false;
    preview.update(&latest);

    let settings = preview.current().expect("preview snapshot exists");
    let providers = selected_providers(
        &settings,
        SuggestionQueryMode::Menu,
        "::",
        false,
        false,
        true,
    );

    assert_eq!(
        providers,
        vec![
            SuggestionProviderKind::Spec,
            SuggestionProviderKind::Snippet
        ]
    );
}

/// 配置进入只读恢复态后，即使预览打开远端来源也只保留本地查询能力。
#[test]
fn read_only_recovery_blocks_preview_selected_remote_providers() {
    let settings = TerminalInlineSuggestionSettings::default();
    let providers = selected_providers(
        &settings,
        SuggestionQueryMode::Inline,
        "git che",
        false,
        true,
        false,
    );

    assert_eq!(
        providers,
        vec![
            SuggestionProviderKind::History,
            SuggestionProviderKind::Spec
        ]
    );
}
