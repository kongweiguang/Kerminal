//! 重启验收的纯判据测试使用合成快照，不伪造真实窗口或插件运行证据。
//! @author kongweiguang

use super::{
    active_renderer, count_result_ready, expected_version, renderer_identity,
    valid_count_before_click, validate_view_semantics,
};
use serde_json::{Value, json};

/// 使用完整的合成控件事实，防止缺少禁用标记或有效布局时被测试辅助值掩盖。
fn button(name: &str) -> Value {
    json!({
        "isButton": true, "layoutVisible": true, "supportsClick": true, "accessibleName": name,
        "accessibilityDisabled": null, "bounds": {"x": 10, "y": 20, "width": 90, "height": 28}
    })
}

/// 基础按钮必须同时存在，才能把新版控件缺失解释为旧版行为而不是空白界面。
fn controls() -> Value {
    json!({
        "save": button("Save"), "check": button("Check example.com"),
        "copy": null, "count": null
    })
}

/// 身份从活动标签读取，避免测试仅比较固定版本标记而漏掉实例或进程串线。
fn snapshot() -> Value {
    json!({
        "activeTabId": "active-tab",
        "tabs": [{"tabId": "active-tab", "renderer": {
            "status": "Ready", "error": null, "plugin": "acme.notes", "view": "notes.editor",
            "instance": "instance-1", "instanceGeneration": 2, "processGeneration": 3
        }}]
    })
}

/// 四个普通进程必须按空根、初装、更新、回退排序，不能把 bootstrap 当成旧版视图。
#[test]
fn phases_keep_bootstrap_and_rollback_distinct() {
    assert_eq!(expected_version(0), None);
    assert_eq!(expected_version(1), Some("1.0.0"));
    assert_eq!(expected_version(2), Some("1.1.0"));
    assert_eq!(expected_version(3), Some("1.0.0"));
}

/// 初装与回退的旧版判据和新版判据互斥，并覆盖缺少真实基础控件的假阴性。
#[test]
fn semantics_require_real_baseline_and_exact_new_controls() {
    let mut observed = controls();
    assert!(validate_view_semantics(&observed, false).is_ok());
    assert!(validate_view_semantics(&observed, true).is_err());
    observed["copy"] = button("Copy note");
    observed["count"] = button("Count code points");
    assert!(validate_view_semantics(&observed, true).is_ok());
    assert!(validate_view_semantics(&observed, false).is_err());
    observed["save"] = Value::Null;
    assert!(validate_view_semantics(&observed, true).is_err());
    observed["copy"] = Value::Null;
    observed["count"] = Value::Null;
    assert!(validate_view_semantics(&observed, false).is_err());
}

/// 单靠存在元素或声明名称不足以证明可交互的 GPUI 按钮已进入当前绘制。
#[test]
fn semantics_reject_wrong_role_name_visibility_and_click() {
    for (field, value) in [
        ("isButton", json!(false)),
        ("layoutVisible", json!(false)),
        ("supportsClick", json!(false)),
        ("accessibilityDisabled", json!(true)),
        ("accessibilityDisabled", json!("false")),
        ("accessibleName", json!("Unrelated")),
    ] {
        let mut observed = controls();
        observed["copy"] = button("Copy note");
        observed["count"] = button("Count code points");
        observed["count"][field] = value;
        assert!(validate_view_semantics(&observed, true).is_err(), "{field}");
    }
}

/// 无障碍禁用状态与 Click 是独立事实，布局必须提供有限的正尺寸，不能只相信 visible。
#[test]
fn semantics_reject_disabled_or_invalid_bounds() {
    let mut observed = controls();
    observed["copy"] = button("Copy note");
    observed["count"] = button("Count code points");
    observed["count"]["accessibilityDisabled"] = json!(false);
    assert!(validate_view_semantics(&observed, true).is_ok());
    observed["count"]["accessibilityDisabled"] = json!(true);
    assert!(validate_view_semantics(&observed, true).is_err());
    observed["count"]
        .as_object_mut()
        .expect("synthetic button object")
        .remove("accessibilityDisabled");
    assert!(validate_view_semantics(&observed, true).is_err());
    observed["count"] = button("Count code points");
    for (field, value) in [
        ("width", json!(0)),
        ("height", json!(-1)),
        ("x", json!("10")),
        ("y", Value::Null),
    ] {
        let mut invalid = observed.clone();
        invalid["count"]["bounds"][field] = value;
        assert!(validate_view_semantics(&invalid, true).is_err(), "{field}");
    }
}

/// 点击前不能已有期望结果；完成必须结合本次输入、插件数据和重新声明 Click 的控件事实。
#[test]
fn count_completion_rejects_stale_or_busy_results() {
    assert!(valid_count_before_click(&json!(""), &json!(""), 4));
    assert!(valid_count_before_click(&json!(4), &json!(4), 6));
    assert!(!valid_count_before_click(&json!(6), &json!(4), 6));
    assert!(!valid_count_before_click(&json!(6), &json!(6), 6));
    let mut renderer = json!({"data": {"text": "note", "codePointCount": 4}});
    let mut count = button("Count code points");
    assert!(count_result_ready(&renderer, &count, "note", 4));
    assert!(!count_result_ready(&renderer, &count, "notes", 4));
    renderer["data"]["codePointCount"] = json!(5);
    assert!(!count_result_ready(&renderer, &count, "note", 4));
    renderer["data"]["codePointCount"] = json!(4);
    count["supportsClick"] = json!(false);
    assert!(!count_result_ready(&renderer, &count, "note", 4));
    count["supportsClick"] = json!(true);
    count["accessibilityDisabled"] = json!(true);
    assert!(!count_result_ready(&renderer, &count, "note", 4));
    count["accessibilityDisabled"] = Value::Null;
    count["bounds"]["width"] = json!(0);
    assert!(!count_result_ready(&renderer, &count, "note", 4));
}

/// 运行代次与 renderer 错误参与门禁，旧实例或假静态渲染不能生成成功回执。
#[test]
fn active_renderer_and_process_identity_are_checked() {
    let mut observed = snapshot();
    let original = renderer_identity(&observed).expect("synthetic identity");
    assert!(active_renderer(&observed).is_ok());
    observed["tabs"][0]["renderer"]["instanceGeneration"] = json!(9);
    assert_ne!(
        renderer_identity(&observed).expect("synthetic identity"),
        original
    );
    observed["tabs"][0]["renderer"]["processGeneration"] = json!(0);
    assert!(renderer_identity(&observed).is_err());
    observed["tabs"][0]["renderer"]["error"] = json!("action failed");
    assert!(active_renderer(&observed).is_err());
    observed["activeTabId"] = json!("other-tab");
    assert!(active_renderer(&observed).is_err());
}
