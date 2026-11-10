//! 扩展页选择只使用当前平台的受信资产和语义版本。
//! @author kongweiguang

use super::{CatalogPlugin, sample, selection};
use kerminal_plugin_protocol::Platform;
use serde_json::json;

/// 较新的不兼容版本不能覆盖当前平台可安装的旧版本。
#[test]
fn selects_latest_compatible_asset_by_semver() {
    let entry: CatalogPlugin = serde_json::from_value(json!({
        "id":"acme.notes", "name":{"en":"Notes"}, "description":{"en":"Notes"},
        "publisher":"acme", "categories":[], "icon_url":null, "readme_url":null,
        "repository":"https://example.com", "permissions":[], "versions":[
            {"version":"1.9.0", "api":"^1.0", "published_at":"2026-01-01", "changelog_url":null,
             "assets":[{"platform":"any", "url":"https://example.com/a", "sha256":"a", "size":1}]},
            {"version":"1.10.0", "api":"^1.0", "published_at":"2026-01-02", "changelog_url":null,
             "assets":[{"platform":"any", "url":"https://example.com/b", "sha256":"b", "size":1}]},
            {"version":"2.0.0", "api":"^2.0", "published_at":"2026-01-03", "changelog_url":null,
             "assets":[{"platform":"any", "url":"https://example.com/c", "sha256":"c", "size":1}]}
        ]
    }))
    .unwrap();
    let selected = selection(&entry, "1.0.0").unwrap();
    assert_eq!(selected.version.to_string(), "1.10.0");
    assert_eq!(selected.platform, Platform::Any);
}

/// 没有匹配资产时不能让用户启动安装。
#[test]
fn missing_compatible_asset_disables_install() {
    let entry: CatalogPlugin = serde_json::from_value(json!({
        "id":"acme.notes", "name":{"en":"Notes"}, "description":{"en":"Notes"},
        "publisher":"acme", "categories":[], "icon_url":null, "readme_url":null,
        "repository":"https://example.com", "permissions":[], "versions":[]
    }))
    .unwrap();
    assert!(selection(&entry, "1.0.0").is_none());
}

/// 内嵌示例必须持续符合 v1 目录契约；字段漂移会在测试阶段暴露而不是渲染成空列表。
#[test]
fn sample_catalog_matches_preview_contract() {
    let catalog = sample::catalog();
    assert_eq!(catalog.schema, 1);
    assert_eq!(catalog.source_id, "example-preview");
    assert_eq!(catalog.sequence, 0);

    let ids: Vec<&str> = catalog
        .plugins
        .iter()
        .map(|plugin| plugin.id.as_str())
        .collect();
    assert_eq!(
        ids,
        ["example.notes", "example.http-check", "example.file-search"]
    );

    for plugin in &catalog.plugins {
        assert!(sample::contains(&plugin.id));
        assert_eq!(plugin.publisher, "example");
        assert_eq!(plugin.categories, ["示例"]);
        assert!(plugin.name.contains_key("zh-CN") && plugin.name.contains_key("en"));
        assert!(plugin.description.contains_key("zh-CN") && plugin.description.contains_key("en"));
        assert!(!plugin.permissions.is_empty());
        assert_eq!(
            plugin.repository,
            "https://github.com/kongweiguang/Kerminal-Plugins"
        );

        let version = plugin.versions.first().expect("示例必须声明版本");
        assert_eq!(version.version.to_string(), "1.0.0");
        assert_eq!(version.api, "^1.0");
        assert_eq!(version.published_at, "2026-11-08");
        // 示例没有可下载资产：URL 指向仓库、摘要全零、大小 1，安装路径不可达。
        for asset in &version.assets {
            assert_eq!(asset.platform, Platform::Any);
            assert_eq!(asset.url, plugin.repository);
            assert_eq!(asset.sha256, "0".repeat(64));
            assert_eq!(asset.size, 1);
        }
        let selected = selection(plugin, "1.0.0").expect("示例应能识别 any 资产");
        assert_eq!(selected.version.to_string(), "1.0.0");
        // API 范围必须真实生效，示例不能绕过宿主版本约束。
        assert!(selection(plugin, "2.0.0").is_none());
    }
}
