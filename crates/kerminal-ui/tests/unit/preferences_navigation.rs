//! GPUI 设置导航与 main 搜索目录的投影回归。
//!
//! @author kongweiguang

use super::{
    PreferencesSection, matches_navigation_query, matching_navigation_targets, navigation_targets,
    resolve_navigation_search_selection,
};

/// 关键词搜索同时匹配中文、英文标签和多词设置名称，空白查询不改变页面。
/// @author kongweiguang
#[test]
fn navigation_search_matches_bilingual_and_multiword_queries() {
    let keywords = "终端 terminal font 字体 size 字号";

    assert!(matches_navigation_query(keywords, "终端"));
    assert!(matches_navigation_query(keywords, "terminal"));
    assert!(matches_navigation_query(keywords, "font size"));
    assert!(!matches_navigation_query(keywords, "vault"));
}

/// 默认导航保留原有页面顺序并加入唯一的 Profile 管理入口。
/// @author kongweiguang
#[test]
fn navigation_exposes_profile_and_existing_pages_in_order() {
    let pages = navigation_targets(None, false)
        .into_iter()
        .filter(|target| target.category)
        .map(|target| target.id)
        .collect::<Vec<_>>();

    assert_eq!(
        pages,
        [
            "appearance",
            "terminal",
            "profiles",
            "keywords",
            "suggestions",
            "sftp",
            "external-launch",
            "mcp",
            "sync",
            "desktop",
            "shortcuts",
            "about",
        ]
    );
}

/// 字段搜索目录和主页面一一对应；标题、描述及关键词均进入同一匹配流程。
/// @author kongweiguang
#[test]
fn main_search_targets_cover_each_settings_page() {
    let targets = navigation_targets(None, false);
    let entries = targets
        .iter()
        .filter(|target| !target.category)
        .collect::<Vec<_>>();

    assert_eq!(entries.len(), 19);
    for (id, section) in [
        (
            "settings-interface-appearance-panel",
            PreferencesSection::Appearance,
        ),
        ("settings-background-panel", PreferencesSection::Appearance),
        (
            "settings-terminal-theme-panel",
            PreferencesSection::Terminal,
        ),
        ("settings-terminal-font-panel", PreferencesSection::Terminal),
        (
            "settings-terminal-renderer-panel",
            PreferencesSection::Terminal,
        ),
        (
            "settings-terminal-interaction-panel",
            PreferencesSection::Terminal,
        ),
        (
            "settings-terminal-cursor-panel",
            PreferencesSection::Terminal,
        ),
        ("settings-profiles-panel", PreferencesSection::Profiles),
        (
            "settings-keyword-highlights-panel",
            PreferencesSection::KeywordHighlights,
        ),
        (
            "settings-command-suggestions-policy-panel",
            PreferencesSection::CommandSuggestions,
        ),
        (
            "settings-command-suggestions-providers-panel",
            PreferencesSection::CommandSuggestions,
        ),
        ("settings-sftp-panel", PreferencesSection::Sftp),
        (
            "settings-external-launch-compatibility",
            PreferencesSection::ExternalLaunch,
        ),
        ("settings-mcp-connection-info", PreferencesSection::Mcp),
        ("settings-sync-panel", PreferencesSection::ConfigurationSync),
        ("settings-desktop-panel", PreferencesSection::Desktop),
        ("settings-keybindings-panel", PreferencesSection::Shortcuts),
        ("settings-about-panel", PreferencesSection::About),
    ] {
        let target = entries
            .iter()
            .find(|target| target.id == id)
            .unwrap_or_else(|| panic!("missing main search target {id}"));
        assert_eq!(target.section, section, "{id}");
        assert!(!target.description.is_empty(), "{id}");
    }
}

/// 具体字段搜索最多展示八项；精确结果不被空查询时的分类条目抢占。
/// @author kongweiguang
#[test]
fn search_filters_pages_and_caps_results_at_eight() {
    let results = matching_navigation_targets(None, false, "a");

    assert!(results.len() <= 8);
    assert!(!results.is_empty());
    assert!(results.iter().all(|target| !target.category));

    let sftp = matching_navigation_targets(None, false, "SFTP");
    assert_eq!(sftp.len(), 1);
    assert_eq!(sftp[0].id, "settings-sftp-panel");
    assert_eq!(sftp[0].section, PreferencesSection::Sftp);
}

/// 应用配色和终端配色是两个可独立定位的设置入口。
///
/// @author kongweiguang
#[test]
fn app_palette_search_finds_the_application_appearance() {
    for query in [
        "应用配色",
        "JetBrains",
        "Fleet",
        "VS Code",
        "vscode",
        "app color scheme",
    ] {
        let results = matching_navigation_targets(None, false, query);
        assert!(
            results.iter().any(|target| {
                target.id == "settings-interface-appearance-panel"
                    && target.section == PreferencesSection::Appearance
            }),
            "{query}"
        );
    }
}

/// MCP 和关于均作为设置页始终可搜索；服务缺失只影响状态，不替换为跳往其它工作区。
/// @author kongweiguang
#[test]
fn mcp_and_about_search_stay_inside_the_settings_pages() {
    for (query, expected_id, section) in [
        (
            "MCP",
            "settings-mcp-connection-info",
            PreferencesSection::Mcp,
        ),
        ("about", "settings-about-panel", PreferencesSection::About),
        ("更新", "settings-about-panel", PreferencesSection::About),
    ] {
        let results = matching_navigation_targets(None, false, query);
        assert!(
            results.iter().any(|target| target.id == expected_id),
            "{query}"
        );

        let selection = resolve_navigation_search_selection(
            query,
            PreferencesSection::Appearance,
            None,
            None,
            false,
        );
        assert!(selection.has_matches, "{query}");
        assert_eq!(selection.section, section, "{query}");
        assert_eq!(
            selection.selected_target.as_deref(),
            Some(expected_id),
            "{query}"
        );
    }
}

/// 页面搜索返回可定位卡片索引，搜索框清空后由 UI 负责执行切页和滚动。
/// @author kongweiguang
#[test]
fn field_search_projects_the_selected_page_and_scroll_target() {
    let selection = resolve_navigation_search_selection(
        "background",
        PreferencesSection::Terminal,
        None,
        None,
        false,
    );

    assert!(selection.has_matches);
    assert_eq!(selection.section, PreferencesSection::Appearance);
    assert_eq!(
        selection.selected_target.as_deref(),
        Some("settings-background-panel")
    );
    assert_eq!(selection.scroll_index, Some(1));
}

/// 清空搜索后恢复包含 Profile 的页面导航。
/// @author kongweiguang
#[test]
fn clearing_search_restores_page_navigation_with_profiles() {
    let categories = matching_navigation_targets(None, false, "")
        .into_iter()
        .filter(|target| target.category)
        .collect::<Vec<_>>();
    assert_eq!(categories.len(), 12);
    assert_eq!(
        categories
            .iter()
            .filter(|target| target.id == "profiles")
            .count(),
        1
    );

    let cleared = resolve_navigation_search_selection(
        "",
        PreferencesSection::Terminal,
        Some("settings-terminal-font-panel"),
        None,
        false,
    );
    assert!(cleared.has_matches);
    assert_eq!(cleared.section, PreferencesSection::Terminal);
    assert_eq!(cleared.selected_target, None);
}
