//! 原生视图投影的只读绑定和草稿隔离回归测试。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, sync::Arc};

use kerminal_plugin_protocol::native::parse_native_view;
use serde_json::json;

use super::super::state::BindingPath;
use super::{NativeViewProjection, display_value};

/// 显隐只接受布尔绑定；远端更新不能覆盖另一个实例里未提交的输入。
#[test]
fn visibility_and_text_follow_each_instances_draft() {
    let view = Arc::new(
        parse_native_view(
            br#"{
        "schema": 1,
        "data": {"visible": true, "form": {"name": "first"}},
        "root": {"type": "text", "text": "$form.name", "visible": "$visible"}
    }"#,
        )
        .expect("valid view"),
    );
    let mut first = NativeViewProjection::new(Arc::clone(&view), 1);
    let second = NativeViewProjection::new(view, 2);
    assert!(first.is_visible(&first.view().root));
    assert_eq!(first.text("$form.name"), "first");
    first
        .state_mut()
        .edit(
            &BindingPath::parse("form.name").expect("path"),
            json!("draft"),
        )
        .expect("edit");
    first
        .apply_patch(&json!({"visible": false, "form": {"name": "remote"}}))
        .expect("patch");
    assert!(!first.is_visible(&first.view().root));
    assert_eq!(first.text("$form.name"), "draft");
    assert!(
        first
            .state()
            .has_new_value(&BindingPath::parse("form.name").expect("path"))
    );
    assert!(second.is_visible(&second.view().root));
    assert_eq!(second.text("$form.name"), "first");
}

/// 不把缺失值、null 或非布尔可见性解释为插件可执行的表达式。
#[test]
fn binding_values_do_not_evaluate_expressions_or_expose_null() {
    let view = Arc::new(
        parse_native_view(
            br#"{
        "schema": 1,
        "data": {"visible": "true"},
        "root": {"type": "text", "text": "literal", "visible": "$visible"}
    }"#,
        )
        .expect("valid view"),
    );
    let projection = NativeViewProjection::new(view, 1);
    assert!(!projection.is_visible(&projection.view().root));
    assert_eq!(projection.text("literal"), "literal");
    assert_eq!(projection.text("$missing"), "");
    assert_eq!(projection.text("$visible || true"), "");
    assert_eq!(display_value(&json!(null)), "");
    assert_eq!(display_value(&json!({"ok": true})), "{\"ok\":true}");
}

/// 语言切换与英文回退都不重建实例，未收录的键保留可诊断的原始标识。
#[test]
fn translates_labels_and_keeps_data_when_locale_changes() {
    let view = Arc::new(
        parse_native_view(
            br#"{"schema":1,"data":{"name":"draft"},"root":{"type":"text","text":"%name%"}}"#,
        )
        .expect("valid view"),
    );
    let translations = Arc::new(BTreeMap::from([
        (
            "en".to_owned(),
            BTreeMap::from([
                ("name".to_owned(), "Name".to_owned()),
                ("help".to_owned(), "Help".to_owned()),
            ]),
        ),
        (
            "zh-CN".to_owned(),
            BTreeMap::from([("name".to_owned(), "名称".to_owned())]),
        ),
    ]));
    let mut projection =
        NativeViewProjection::new(view, 1).with_translations(translations, "zh-CN");
    assert_eq!(projection.text("%name%"), "名称");
    assert_eq!(projection.text("%help%"), "Help");
    assert_eq!(projection.text("%missing%"), "%missing%");
    projection.set_locale("en");
    assert_eq!(projection.text("%name%"), "Name");
    assert_eq!(projection.text("$name"), "draft");
}

/// 链接按有效权限及宿主相同规则判定，不能通过自定义协议或 URL 用户凭据绕过。
#[test]
fn external_links_require_permission_and_https_without_credentials() {
    use super::allowed_external_url;
    use kerminal_plugin_protocol::Permission;
    use std::collections::BTreeSet;

    let none = BTreeSet::new();
    let allowed = BTreeSet::from([Permission::OpenExternal]);
    let full = BTreeSet::from([Permission::FullAccess]);
    assert!(allowed_external_url(&none, "https://example.com").is_none());
    assert_eq!(
        allowed_external_url(&allowed, "https://example.com/docs")
            .unwrap()
            .as_str(),
        "https://example.com/docs"
    );
    assert!(allowed_external_url(&full, "https://example.com").is_some());
    for url in [
        "http://example.com",
        "file:///private",
        "javascript:alert(1)",
        "https://user:pass@example.com",
        "https://user@example.com",
        "https:///",
        "not-a-url",
    ] {
        assert!(allowed_external_url(&allowed, url).is_none(), "{url}");
    }
}

/// 重载静态节点后仍保留未确认输入，并接受新进程快照作为远端基线。
#[test]
fn replacing_view_preserves_draft_across_new_remote_snapshot() {
    let old = Arc::new(
        parse_native_view(
            br#"{"schema":1,"data":{"name":"old"},"root":{"type":"text","text":"old"}}"#,
        )
        .unwrap(),
    );
    let new = Arc::new(
        parse_native_view(
            br#"{"schema":1,"data":{"name":"new"},"root":{"type":"text","text":"new"}}"#,
        )
        .unwrap(),
    );
    let mut projection = NativeViewProjection::new(old, 1);
    let path = BindingPath::parse("name").unwrap();
    projection.state_mut().edit(&path, json!("draft")).unwrap();
    projection.replace_view(new, Arc::new(BTreeMap::new()));
    projection
        .state_mut()
        .replace_remote(json!({"name":"new"}))
        .unwrap();
    assert_eq!(projection.text("$name"), "draft");
    assert!(projection.state().is_dirty(&path));
    assert!(projection.state().has_new_value(&path));
}
