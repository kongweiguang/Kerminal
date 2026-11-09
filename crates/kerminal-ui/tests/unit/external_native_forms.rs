//! 验证普通原生表单的绑定范围和 schema 校验边界。
//!
//! @author kongweiguang

use super::NativeForm;
use kerminal_plugin_protocol::{ConfigurationSchema, native::NativeNodeKind};
use serde_json::json;

/// schema 字段映射到声明的子对象，提交快照不受无关视图数据影响。
#[test]
fn form_projects_fields_under_binding_and_validates_only_that_object() {
    let schema = ConfigurationSchema {
        document: json!({
            "type":"object", "required":["name", "enabled"], "additionalProperties":false,
            "properties": {"name":{"type":"string", "minLength":2}, "enabled":{"type":"boolean"}}
        }),
    };
    let form = NativeForm::from_schema(schema, Some("panel.config")).unwrap();
    let NativeNodeKind::Stack { children, .. } = &form.root.kind else {
        panic!("expected form stack")
    };
    assert!(children.iter().any(|node| matches!(&node.kind, NativeNodeKind::Input { bind, .. } if bind == "panel.config.name")));
    assert!(children.iter().any(|node| matches!(&node.kind, NativeNodeKind::Switch { bind, .. } if bind == "panel.config.enabled")));
    let (schema, value) = form.validation_snapshot(
        &json!({"panel":{"config":{"name":"ok", "enabled":true}}, "other":"ignored"}),
    );
    assert!(schema.validate(&value).is_ok());
    let (schema, value) =
        form.validation_snapshot(&json!({"panel":{"config":{"name":"x", "enabled":true}}}));
    assert!(schema.validate(&value).is_err());
}

/// 不可表示的对象键及秘密字段不能被映射到错误路径或普通明文输入。
#[test]
fn form_rejects_ambiguous_paths_and_secret_fields() {
    for document in [
        json!({"type":"object", "properties":{"one.two":{"type":"string"}}}),
        json!({"type":"object", "properties":{"token":{"type":"string", "x-secret":true}}}),
        json!({"type":"object", "properties":{"items":{"type":"array"}}}),
    ] {
        assert!(NativeForm::from_schema(ConfigurationSchema { document }, Some("panel")).is_err());
    }
}
