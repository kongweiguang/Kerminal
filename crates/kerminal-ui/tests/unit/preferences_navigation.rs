//! 偏好导航搜索的中英文关键词与多词查询回归测试。
//!
//! @author kongweiguang

use super::{
    NavigationBodyProjection, PreferencesNavigationDestination, PreferencesSection,
    matches_navigation_query, matching_navigation_targets, navigation_targets,
    project_navigation_body, resolve_navigation_search_selection,
};

/// 关键词搜索同时覆盖中文同义词、英语标签和多词设置名称。
#[test]
fn navigation_search_matches_bilingual_and_multiword_queries() {
    let keywords = "终端 terminal font 字体 size 字号";

    assert!(matches_navigation_query(keywords, "终端"));
    assert!(matches_navigation_query(keywords, "terminal"));
    assert!(matches_navigation_query(keywords, "font size"));
    assert!(!matches_navigation_query(keywords, "vault"));
}

/// 常见滚动缓冲说法应定位到实际字体与滚屏设置卡片，而不是只命中终端分类。
///
/// @author kongweiguang
#[test]
fn scrollback_synonyms_route_to_the_font_and_scrollback_card() {
    for query in ["字体与滚动", "滚动", "滚屏", "缓冲"] {
        let selection = resolve_navigation_search_selection(
            query,
            PreferencesSection::Appearance,
            None,
            None,
            false,
        );

        assert!(selection.has_matches, "expected a result for {query}");
        assert_eq!(selection.section, PreferencesSection::Terminal, "{query}");
        assert_eq!(
            selection.selected_target.as_deref(),
            Some("terminal-font"),
            "{query}"
        );
        assert_eq!(selection.scroll_index, Some(1), "{query}");
        assert_eq!(selection.destination, None, "{query}");
        assert_eq!(
            project_navigation_body(query, &selection),
            NavigationBodyProjection::Section(PreferencesSection::Terminal),
            "{query}"
        );
    }
}

/// 空查询保留完整导航，避免清除搜索时菜单仍停留在过滤态。
#[test]
fn empty_navigation_query_keeps_all_sections_visible() {
    assert!(matches_navigation_query("appearance theme", ""));
    assert!(matches_navigation_query("appearance theme", "   "));
}

/// 字段级结果映射到各滚动页的真实卡片位置，避免搜索只切换大类却不定位。
///
/// @author kongweiguang
#[test]
fn navigation_targets_point_to_the_expected_settings_groups() {
    let targets = navigation_targets(None, false);
    let target = |id: &str| targets.iter().find(|target| target.id == id).unwrap();

    assert_eq!(
        target("appearance-background").section,
        PreferencesSection::Appearance
    );
    assert_eq!(target("appearance-background").scroll_index, Some(1));
    assert_eq!(target("terminal-behavior").scroll_index, Some(3));
    assert_eq!(target("integrations-sftp").scroll_index, Some(3));
    assert_eq!(target("integrations-protocol").scroll_index, Some(6));
    assert_eq!(target("sync-actions").scroll_index, Some(0));
}

/// 本地主机的管理及默认项已归入连接管理，设置分类和搜索不能残留独立 Profile 入口。
/// @author kongweiguang
#[test]
fn local_host_management_is_not_duplicated_in_settings_navigation() {
    let targets = navigation_targets(None, false);
    assert!(
        targets
            .iter()
            .all(|target| target.section != PreferencesSection::Profiles)
    );
    for id in [
        "category-profiles",
        "profile-shell",
        "profile-directory-env",
    ] {
        assert!(!targets.iter().any(|target| target.id == id));
    }
    for query in ["profile", "本地 Profile"] {
        assert!(
            matching_navigation_targets(None, false, query)
                .iter()
                .all(|target| target.section != PreferencesSection::Profiles)
        );
    }
}

/// MCP 查询只命中真实运行时目标；未装配时不把通知集成页冒充为 MCP 设置。
#[test]
fn mcp_search_routes_only_to_the_existing_runtime_when_available() {
    let unavailable = navigation_targets(None, false);
    assert!(!unavailable.iter().any(|target| target.id == "mcp-runtime"));
    let integration_category = unavailable
        .iter()
        .find(|target| target.id == "category-integrations")
        .unwrap();
    assert!(!matches_navigation_query(
        &integration_category.keywords,
        "mcp"
    ));

    let available = navigation_targets(None, true);
    let runtime = available
        .iter()
        .find(|target| target.id == "mcp-runtime")
        .unwrap();
    assert!(matches_navigation_query(&runtime.keywords, "mcp"));
    assert_eq!(
        runtime.destination,
        Some(PreferencesNavigationDestination::McpRuntime)
    );
    let visible_results = matching_navigation_targets(None, true, "MCP");
    assert_eq!(visible_results.len(), 1);
    assert_eq!(visible_results[0].id, "mcp-runtime");
}

/// “关于与更新”始终可搜索，并只投影到受控更新工作区目标，不触发更新副作用。
///
/// @author kongweiguang
#[test]
fn about_and_updates_search_routes_to_the_existing_updates_workspace() {
    let targets = navigation_targets(None, false);
    let updates = targets
        .iter()
        .find(|target| target.id == "about-updates")
        .unwrap();
    assert_eq!(updates.section, PreferencesSection::Appearance);
    assert_eq!(
        updates.destination,
        Some(PreferencesNavigationDestination::Updates)
    );

    for query in ["about", "更新", "version"] {
        let visible = matching_navigation_targets(None, false, query);
        assert!(visible.iter().any(|target| target.id == "about-updates"));
    }

    let selection = resolve_navigation_search_selection(
        "about",
        PreferencesSection::Terminal,
        None,
        None,
        false,
    );
    assert_eq!(selection.section, PreferencesSection::Appearance);
    assert_eq!(selection.selected_target.as_deref(), Some("about-updates"));
    assert_eq!(
        selection.destination,
        Some(PreferencesNavigationDestination::Updates)
    );
    assert_eq!(
        project_navigation_body("about", &selection),
        NavigationBodyProjection::Destination(PreferencesNavigationDestination::Updates)
    );
}

/// 有匹配项且原页面已被过滤时，导航和正文状态一起切到首个实际命中页。
///
/// @author kongweiguang
#[test]
fn search_moves_filtered_out_page_to_first_matching_section() {
    let selection = resolve_navigation_search_selection(
        "MCP",
        PreferencesSection::Appearance,
        None,
        None,
        true,
    );

    assert!(selection.has_matches);
    assert_eq!(selection.section, PreferencesSection::Integrations);
    assert_eq!(selection.selected_target.as_deref(), Some("mcp-runtime"));
    assert_eq!(
        selection.destination,
        Some(PreferencesNavigationDestination::McpRuntime)
    );
    assert_eq!(selection.scroll_index, None);
    assert_eq!(
        project_navigation_body("MCP", &selection),
        NavigationBodyProjection::Destination(PreferencesNavigationDestination::McpRuntime)
    );
}

/// 字段搜索选择具体卡片索引，正文可在现有滚动容器中定位而不重建输入控件。
///
/// @author kongweiguang
#[test]
fn field_search_projects_its_real_scroll_target() {
    let selection = resolve_navigation_search_selection(
        "background",
        PreferencesSection::Appearance,
        None,
        None,
        false,
    );

    assert!(selection.has_matches);
    assert_eq!(selection.section, PreferencesSection::Appearance);
    assert_eq!(
        selection.selected_target.as_deref(),
        Some("appearance-background")
    );
    assert_eq!(selection.scroll_index, Some(1));
    assert_eq!(selection.destination, None);
    assert_eq!(
        project_navigation_body("background", &selection),
        NavigationBodyProjection::Section(PreferencesSection::Appearance)
    );
    let visible_results = matching_navigation_targets(None, false, "background");
    assert!(
        visible_results
            .iter()
            .any(|target| target.id == "appearance-background")
    );
    assert!(
        !visible_results
            .iter()
            .any(|target| target.id == "category-appearance")
    );
}

/// 当前 section 仍有可见结果时不跳走；清空搜索后保留该页并回到普通分类导航。
///
/// @author kongweiguang
#[test]
fn search_keeps_valid_section_and_clearing_query_keeps_selected_page() {
    let matching = resolve_navigation_search_selection(
        "theme",
        PreferencesSection::Appearance,
        None,
        None,
        false,
    );
    assert!(matching.has_matches);
    assert_eq!(matching.section, PreferencesSection::Appearance);
    assert_eq!(
        matching.selected_target.as_deref(),
        Some("appearance-theme")
    );

    let cleared = resolve_navigation_search_selection(
        "",
        matching.section,
        matching.selected_target.as_deref(),
        None,
        false,
    );
    assert!(cleared.has_matches);
    assert_eq!(cleared.section, PreferencesSection::Appearance);
    assert_eq!(cleared.selected_target, None);
    assert_eq!(
        project_navigation_body("", &cleared),
        NavigationBodyProjection::Section(PreferencesSection::Appearance)
    );
}

/// 无匹配结果保留原草稿页状态，但用显式标志让渲染隐藏旧内容并显示空态。
///
/// @author kongweiguang
#[test]
fn no_search_matches_keep_selection_state_for_empty_results_rendering() {
    let selection = resolve_navigation_search_selection(
        "no-such-setting",
        PreferencesSection::Terminal,
        Some("terminal-font"),
        None,
        false,
    );

    assert!(!selection.has_matches);
    assert_eq!(selection.section, PreferencesSection::Terminal);
    assert_eq!(selection.selected_target, None);
    assert_eq!(
        project_navigation_body("no-such-setting", &selection),
        NavigationBodyProjection::NoResults
    );
}
