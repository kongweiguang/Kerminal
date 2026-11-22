//! 验证 Vault 写入期间的秘密草稿与重复操作门禁，以及运行时下拉选项的输入规则。
//! @author kongweiguang

use std::collections::BTreeMap;

use serde_json::json;

use super::SecretWriteState;

/// 成功回执只可清空尚未再编辑的输入，并允许之后再次保存。
#[test]
fn secret_write_preserves_edits_and_rejects_overlapping_writes() {
    let state = SecretWriteState::default();
    assert!(state.begin());
    assert!(state.is_pending());
    assert!(!state.begin());
    state.edited();
    assert!(!state.finish());
    assert!(!state.is_pending());
    assert!(state.begin());
    assert!(state.finish());
    assert!(!state.is_pending());
}

/// 扩展只接受带 `$` 的合法只读绑定；缺少前缀、空路径或非法段都回退普通输入。
#[test]
fn runtime_select_extension_requires_a_read_only_binding() {
    let mut extensions = BTreeMap::new();
    extensions.insert(
        "x-kerminal-select-options".into(),
        json!("$address_options"),
    );
    assert_eq!(
        super::runtime_select_binding(&extensions).as_deref(),
        Some("address_options")
    );

    for invalid in [
        json!("address_options"),
        json!("$"),
        json!("$address..options"),
        json!("$address.options!"),
        json!(7),
    ] {
        extensions.insert("x-kerminal-select-options".into(), invalid);
        assert!(super::runtime_select_binding(&extensions).is_none());
    }

    extensions.remove("x-kerminal-select-options");
    assert!(super::runtime_select_binding(&extensions).is_none());
}

/// 运行时候选只接受非空字符串标签和值；重复值保留首个，避免选择位置歧义。
#[test]
fn runtime_choices_skip_invalid_and_duplicate_rows() {
    let value = json!([
        {"label":"Ethernet","value":"192.168.1.10"},
        {"label":"duplicate","value":"192.168.1.10"},
        {"label":"Wi-Fi","value":"10.0.0.2"},
        {"label":"","value":"empty-label"},
        {"label":"empty-value","value":""},
        {"label":"wrong-value-type","value":7},
        {"label":"wrong-label-type","value":["192.168.1.11"]},
        {"label":"array-row"},
        7
    ]);
    let choices = super::runtime_select_choices(Some(&value));
    assert_eq!(choices.len(), 2);
    assert_eq!(choices[0].index, "192.168.1.10");
    assert_eq!(choices[0].label.as_ref(), "Ethernet");
    assert_eq!(choices[1].index, "10.0.0.2");
    assert!(super::runtime_select_choices(None).is_empty());
    assert!(super::runtime_select_choices(Some(&json!({}))).is_empty());
}

/// 超过上限的补丁只投影前 500 项，避免一次性构造无界下拉列表。
#[test]
fn runtime_choices_are_bounded() {
    let rows = (0..super::MAX_RUNTIME_SELECT_OPTIONS + 10)
        .map(|index| json!({"label": format!("address-{index}"), "value": format!("10.0.0.{index}")}))
        .collect::<Vec<_>>();
    let choices = super::runtime_select_choices(Some(&json!(rows)));
    assert_eq!(choices.len(), super::MAX_RUNTIME_SELECT_OPTIONS);
}
