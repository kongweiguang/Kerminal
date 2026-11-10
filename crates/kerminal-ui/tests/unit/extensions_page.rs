//! 扩展页选择只使用当前平台的受信资产和语义版本。
//! @author kongweiguang

use super::{CatalogPlugin, selection};
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
