//! Terminal inline suggestion preview policy regression coverage.
//!
//! @author kongweiguang

use super::*;
use crate::commands::SuggestionSettingsPreview;

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
    let providers = selected_providers(&settings, SuggestionQueryMode::Menu, "::", false, true);

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
