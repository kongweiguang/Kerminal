//! 插件设置 schema 投影与对象路径写入的边界测试。
//!
//! @author kongweiguang

use super::{ExternalSettingsCloseState, FieldKind, collect_fields, set_field};
use serde_json::json;

/// 带斜杠和波浪号的字段必须使用 JSON Pointer 转义，不能和其他字段串位。
#[test]
fn nested_fields_keep_escaped_json_pointers_and_secret_types() {
    let schema = json!({"type":"object", "properties": {
        "auth/name": {"type":"object", "properties": {
            "api~token": {"type":"string", "x-secret":true},
            "enabled": {"type":"boolean"},
            "mode": {"type":"string", "enum":["one", "two"]}
        }}
    }});
    let mut fields = Vec::new();
    collect_fields(&schema, &mut Vec::new(), &mut fields).unwrap();
    let secret = fields
        .iter()
        .find(|field| field.pointer == "/auth~1name/api~0token")
        .unwrap();
    assert!(matches!(secret.kind, FieldKind::Secret));
    assert!(
        fields
            .iter()
            .any(|field| field.pointer == "/auth~1name/enabled"
                && matches!(field.kind, FieldKind::Boolean))
    );
    assert!(
        fields
            .iter()
            .any(|field| field.pointer == "/auth~1name/mode"
                && matches!(&field.kind, FieldKind::Choice(values) if values.len() == 2))
    );
    let mut document = json!({});
    set_field(&mut document, &secret.segments, json!(null)).unwrap();
    assert_eq!(document.pointer(&secret.pointer), Some(&json!(null)));
}

/// 复杂组合不能显示成缺字段的表单，非对象父节点也不能被覆盖。
#[test]
fn rejects_unrepresentable_schema_and_preserves_existing_values() {
    for schema in [
        json!({"type":"object", "properties": {"value": {"$ref":"#/defs/value"}}}),
        json!({"type":"object", "properties": {"value": {"oneOf":[{"type":"string"},{"type":"number"}]}}}),
        json!({"type":"object", "additionalProperties":{"type":"string"}, "properties":{}}),
        json!({"type":"object", "properties": {"password": {"type":"number", "x-secret":true}}}),
    ] {
        assert!(collect_fields(&schema, &mut Vec::new(), &mut Vec::new()).is_err());
    }
    let mut document = json!({"nested":"preserve"});
    assert!(set_field(&mut document, &["nested".into(), "value".into()], json!(1)).is_err());
    assert_eq!(document, json!({"nested":"preserve"}));
}

/// 无效输入也属于未保存草稿；仅用户确认丢弃可越过普通草稿门禁。
#[test]
fn close_requires_an_explicit_discard_for_edited_or_invalid_fields() {
    use ExternalSettingsCloseState as State;

    let clean = State::for_form(false, None, false, false);
    assert_eq!(clean, State::Ready);
    assert!(clean.ensure_allowed(false).is_ok());
    for (dirty, invalid) in [(true, false), (false, true), (true, true)] {
        let state = State::for_form(false, None, dirty, invalid);
        assert_eq!(state, State::UnsavedChanges);
        assert!(state.ensure_allowed(false).is_err());
        assert!(state.ensure_allowed(true).is_ok());
    }
}

/// 在途保存和未知回执不能被丢弃选择绕开，回读完成后才重新判断草稿。
#[test]
fn close_waits_for_operations_and_mandatory_readback() {
    use ExternalSettingsCloseState as State;
    use kerminal_application::PublicErrorCode;

    let saving = State::for_form(
        true,
        Some(super::SettingsReadbackReason::OutcomeUnknown),
        true,
        true,
    );
    assert_eq!(saving, State::Working);
    assert!(saving.ensure_allowed(true).is_err());

    let conflict = State::for_form(
        false,
        Some(super::SettingsReadbackReason::Conflict),
        true,
        false,
    );
    assert_eq!(conflict, State::ReadbackRequired);
    assert_eq!(
        conflict.ensure_allowed(true).unwrap_err().public_code(),
        PublicErrorCode::Conflict
    );

    let unknown = State::for_form(
        false,
        Some(super::SettingsReadbackReason::OutcomeUnknown),
        true,
        false,
    );
    assert_eq!(unknown, State::OutcomeUnknown);
    assert_eq!(
        unknown.ensure_allowed(true).unwrap_err().public_code(),
        PublicErrorCode::OutcomeUnknown
    );
    assert_eq!(
        State::for_form(false, None, true, false),
        State::UnsavedChanges
    );
    assert!(
        State::for_form(false, None, false, false)
            .ensure_allowed(false)
            .is_ok()
    );
}

/// 原生插件没有网页资源时也必须使用已校验的设置 schema，缺失声明则明确失败。
#[test]
fn native_settings_use_validated_package_schema_without_web_resources() {
    use kerminal_plugin_protocol::{
        ConfigurationSchema, ManifestPolicy, ValidatedPackage, parse_manifest,
    };
    use std::collections::BTreeMap;

    let manifest = json!({
        "schema": 1, "id": "acme.notes", "name": {"en": "Notes"},
        "description": {"en": "Notes"}, "version": "1.0.0", "api": "^1.0",
        "publisher": "acme", "runtime": {"kind": "none"}
    });
    let manifest = parse_manifest(
        &serde_json::to_vec(&manifest).unwrap(),
        ManifestPolicy::ThirdParty,
    )
    .unwrap();
    let schema = ConfigurationSchema::parse(
        br#"{"type":"object","properties":{"showPreview":{"type":"boolean"}}}"#,
    )
    .unwrap();
    let package = ValidatedPackage {
        manifest,
        files: BTreeMap::new(),
        views: BTreeMap::new(),
        configuration_schemas: BTreeMap::from([("schemas/settings.json".into(), schema)]),
    };
    let loaded = super::settings_schema(&package, "schemas/settings.json").unwrap();
    let mut fields = Vec::new();
    collect_fields(&loaded.document, &mut Vec::new(), &mut fields).unwrap();
    assert!(fields.iter().any(|field| field.pointer == "/showPreview"));
    assert!(super::settings_schema(&package, "schemas/missing.json").is_err());
}

/// 最后的删除操作覆盖已有输入；撤销删除或再次输入则恢复替换意图。
#[test]
fn secret_edit_respects_the_latest_remove_or_input_intent() {
    use kerminal_application::plugin_settings::PluginSecretEdit;
    use zeroize::Zeroizing;

    assert!(matches!(
        super::secret_edit(Zeroizing::new("replacement".into()), true),
        Some(PluginSecretEdit::Remove)
    ));
    assert!(matches!(
        super::secret_edit(Zeroizing::new("replacement".into()), false),
        Some(PluginSecretEdit::Replace(value)) if value.as_str() == "replacement"
    ));
    assert!(matches!(
        super::secret_edit(Zeroizing::new(String::new()), true),
        Some(PluginSecretEdit::Remove)
    ));
    assert!(super::secret_edit(Zeroizing::new(String::new()), false).is_none());
}

/// 有符号边界以上和 u64 上限都应保持整数精度，超出范围的输入必须拒绝。
#[test]
fn integer_input_preserves_full_json_number_range() {
    assert_eq!(
        super::parse_integer("9223372036854775808"),
        Some(json!(9223372036854775808_u64))
    );
    assert_eq!(
        super::parse_integer("18446744073709551615"),
        Some(json!(u64::MAX))
    );
    assert_eq!(
        super::parse_integer("-9223372036854775808"),
        Some(json!(i64::MIN))
    );
    assert!(super::parse_integer("18446744073709551616").is_none());
    assert!(super::parse_integer("-9223372036854775809").is_none());
}
