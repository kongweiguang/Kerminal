//! 冻结的 command-specs/v1 资源及静态建议行为测试。
//!
//! @author kongweiguang

use kerminal_application::commands::CommandSuggestionProvider;
use kerminal_domain::commands::{
    CommandHistoryTarget, CommandSuggestionRequest, SuggestionPresentation, SuggestionProviderKind,
    SuggestionQueryMode,
};
use kerminal_infrastructure::commands::StaticSpecSuggestionProvider;
use tokio_util::sync::CancellationToken;

/// 保证所有冻结资源都能被程序内置解析，避免发行后才发现某个 TOML 没进入二进制。
#[test]
fn embeds_all_frozen_v1_specs() {
    let provider = StaticSpecSuggestionProvider::new().expect("frozen specs should parse");

    assert_eq!(provider.command_count(), 49);
    assert_eq!(
        provider.supported_providers(),
        vec![SuggestionProviderKind::Spec]
    );
}

/// 验证静态枚举只在声明的参数位置补全，不能把资源名重复建议为后续自由参数。
#[tokio::test]
async fn enum_values_are_limited_to_their_declared_position() {
    let provider = StaticSpecSuggestionProvider::new().expect("frozen specs should parse");
    let request = suggestion_request("kubectl delete p", "bash", SuggestionQueryMode::Menu);
    let candidates = provider
        .suggest(request, CancellationToken::new())
        .await
        .expect("enum query should succeed");
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.display_text == "pods")
    );
    let pods = candidates
        .iter()
        .find(|candidate| candidate.display_text == "pods")
        .expect("resource enum value should be present in its menu");
    assert_eq!(
        pods.sensitivity,
        kerminal_domain::commands::CommandSuggestionSensitivity::Dangerous
    );
    assert_eq!(
        pods.allowed_presentations,
        vec![SuggestionPresentation::Menu]
    );

    let request = suggestion_request("kubectl delete p", "bash", SuggestionQueryMode::Inline);
    let candidates = provider
        .suggest(request, CancellationToken::new())
        .await
        .expect("inline query should succeed");
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.display_text != "pods")
    );

    let request = suggestion_request("kubectl delete pods ", "bash", SuggestionQueryMode::Menu);
    let candidates = provider
        .suggest(request, CancellationToken::new())
        .await
        .expect("following argument query should succeed");
    assert!(candidates.iter().all(|candidate| {
        !["configmaps", "deployments", "pods", "services"]
            .contains(&candidate.display_text.as_str())
    }));
}

/// 防止换行或控制字符把另一条命令伪装成当前命令的静态补全上下文。
#[tokio::test]
async fn control_characters_suppress_static_suggestions() {
    let provider = StaticSpecSuggestionProvider::new().expect("frozen specs should parse");
    let request = suggestion_request("kubectl\nget", "bash", SuggestionQueryMode::Menu);
    let candidates = provider
        .suggest(request, CancellationToken::new())
        .await
        .expect("invalid syntax is an empty query, not a provider failure");
    assert!(candidates.is_empty());
}

/// 验证危险子命令只出现在主动菜单，且多字节前缀的替换范围按 Unicode 标量计数。
#[tokio::test]
async fn dangerous_subcommands_are_menu_only_and_ranges_use_scalars() {
    let provider = StaticSpecSuggestionProvider::new().expect("frozen specs should parse");
    let mut request = suggestion_request("kubectl del", "bash", SuggestionQueryMode::Menu);

    let menu = provider
        .suggest(request.clone(), CancellationToken::new())
        .await
        .expect("menu query should succeed");
    let delete = menu
        .iter()
        .find(|candidate| candidate.display_text == "delete")
        .expect("kubectl delete should be present in the frozen spec");
    assert_eq!(
        delete.sensitivity,
        kerminal_domain::commands::CommandSuggestionSensitivity::Dangerous
    );
    assert_eq!(delete.allowed_presentations, [SuggestionPresentation::Menu]);
    assert_eq!(delete.replacement_range.start, 8);
    assert_eq!(delete.replacement_range.end, 11);

    request.mode = SuggestionQueryMode::Inline;
    let inline = provider
        .suggest(request, CancellationToken::new())
        .await
        .expect("inline query should succeed");
    assert!(
        inline
            .iter()
            .all(|candidate| candidate.display_text != "delete")
    );
}

/// 创建不依赖用户数据或当前终端的静态建议请求。
fn suggestion_request(
    input: &str,
    shell: &str,
    mode: SuggestionQueryMode,
) -> CommandSuggestionRequest {
    CommandSuggestionRequest {
        input: input.to_owned(),
        cursor: input.chars().count(),
        target: CommandHistoryTarget::Ssh,
        session_id: Some("session-test".to_owned()),
        pane_id: Some("pane-test".to_owned()),
        profile_id: None,
        remote_host_id: Some("host-test".to_owned()),
        cwd: None,
        shell: Some(shell.to_owned()),
        providers: Some(vec![SuggestionProviderKind::Spec]),
        limit: Some(50),
        mode,
        generation: Some(1),
        context_key: Some("spec-test".to_owned()),
    }
}
