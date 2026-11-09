//! 外部主机自动表单拒绝无法可靠编辑的 schema。
//! @author kongweiguang

use super::{collect_fields, validate_host_form_schema};
use serde_json::json;

/// 组合、引用和动态对象必须显式拒绝，避免把未展示字段静默覆盖。
#[test]
fn unsupported_host_schema_is_rejected() {
    for schema in [
        json!({"type":"object","properties":{"x":{"$ref":"#/$defs/x"}}}),
        json!({"type":"object","properties":{"x":{"oneOf":[{"type":"string"},{"type":"number"}]}}}),
        json!({"type":"object","properties":{},"unevaluatedProperties":false}),
        json!({"type":"object","properties":{},"additionalProperties":true}),
        json!({"type":"object","properties":{"x":{"type":"object","x-secret":true,"properties":{"v":{"type":"string"}}}}}),
    ] {
        assert!(
            validate_host_form_schema(&schema)
                .and_then(|_| collect_fields(&schema, &mut Vec::new(), &mut Vec::new()))
                .is_err()
        );
    }
}

/// 普通静态字符串与秘密字段依然支持，不要求未显式声明的属性策略。
#[test]
fn static_host_schema_is_supported() {
    let schema = json!({"type":"object","properties":{"url":{"type":"string"},"token":{"type":"string","x-secret":true}}});
    validate_host_form_schema(&schema).unwrap();
    let mut fields = Vec::new();
    collect_fields(&schema, &mut Vec::new(), &mut fields).unwrap();
    assert_eq!(fields.len(), 2);
}
