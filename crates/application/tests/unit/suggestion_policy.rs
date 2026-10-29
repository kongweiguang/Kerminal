//! Application command suggestion settings policy regression coverage.
//!
//! @author kongweiguang

use super::*;

/// 构造明确 SSH 目标，避免测试把远端来源错归为活动 session 推断。
fn ssh_request(providers: Option<Vec<SuggestionProviderKind>>) -> CommandSuggestionRequest {
    CommandSuggestionRequest {
        input: "git che".to_owned(),
        cursor: 7,
        target: CommandHistoryTarget::Ssh,
        session_id: Some("session-test".to_owned()),
        pane_id: None,
        profile_id: None,
        remote_host_id: Some("host-test".to_owned()),
        cwd: Some("/workspace".to_owned()),
        shell: Some("bash".to_owned()),
        providers,
        limit: Some(8),
        mode: kerminal_domain::commands::SuggestionQueryMode::Menu,
        generation: Some(1),
        context_key: Some("context-test".to_owned()),
    }
}

/// 通用建议按磁盘持久来源开关过滤，显式请求的 History 与 Snippet 保持原样。
#[test]
fn generic_suggestions_apply_persisted_remote_provider_settings() {
    let mut settings = TerminalInlineSuggestionSettings::default();
    settings.providers.remote_path = false;
    let mut request = ssh_request(Some(vec![
        SuggestionProviderKind::History,
        SuggestionProviderKind::Snippet,
        SuggestionProviderKind::RemotePath,
        SuggestionProviderKind::RemoteCommand,
        SuggestionProviderKind::Git,
    ]));
    let supported = vec![
        SuggestionProviderKind::History,
        SuggestionProviderKind::Snippet,
        SuggestionProviderKind::RemotePath,
        SuggestionProviderKind::RemoteCommand,
        SuggestionProviderKind::Git,
    ];

    assert!(request_has_remote_provider(&request, &supported));
    restrict_remote_providers_to_persisted_settings(&mut request, supported, Some(&settings), true);

    assert_eq!(
        request.providers,
        Some(vec![
            SuggestionProviderKind::History,
            SuggestionProviderKind::Snippet,
            SuggestionProviderKind::RemoteCommand,
            SuggestionProviderKind::Git,
        ])
    );
}

/// 只读恢复硬门禁无需读取损坏设置，也会从通用请求中移除所有远端来源。
#[test]
fn generic_suggestions_drop_remote_providers_during_read_only_recovery() {
    let mut request = ssh_request(None);
    let supported = vec![
        SuggestionProviderKind::History,
        SuggestionProviderKind::Snippet,
        SuggestionProviderKind::RemotePath,
        SuggestionProviderKind::RemoteCommand,
        SuggestionProviderKind::Git,
    ];

    restrict_remote_providers_to_persisted_settings(&mut request, supported, None, false);

    assert_eq!(
        request.providers,
        Some(vec![
            SuggestionProviderKind::History,
            SuggestionProviderKind::Snippet
        ])
    );
}
