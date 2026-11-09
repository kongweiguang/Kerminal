//! 设置和主机字段采用标准 JSON Schema，并禁止校验期间读取外部资源。
//! @author kongweiguang

use serde_json::Value;

use crate::{ProtocolError, invalid, parse_json};

pub const MAX_CONFIGURATION_SCHEMA_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct ConfigurationSchema {
    pub document: Value,
}

impl ConfigurationSchema {
    /// 安装、启动和离线配置扫描共同编译同一份 schema，不运行插件进程。
    pub fn parse(bytes: &[u8]) -> Result<Self, ProtocolError> {
        if bytes.len() > MAX_CONFIGURATION_SCHEMA_BYTES {
            return Err(invalid("configuration.schema", "配置 schema 超过大小上限"));
        }
        let document = parse_json(bytes, "configuration.schema")?;
        if !document.is_object() {
            return Err(invalid("configuration.schema", "配置 schema 必须是对象"));
        }
        validate_schema_references(&document, 0)?;
        jsonschema::meta::validate(&document)
            .map_err(|_| invalid("configuration.schema", "JSON Schema 定义无效"))?;
        jsonschema::validator_for(&document)
            .map_err(|_| invalid("configuration.schema", "JSON Schema 无法编译或引用不可用"))?;
        Ok(Self { document })
    }

    /// 数据由标准验证器判断，错误不回显可能包含凭据的字段内容。
    pub fn validate(&self, value: &Value) -> Result<(), ProtocolError> {
        let validator = jsonschema::validator_for(&self.document)
            .map_err(|_| invalid("configuration.schema", "JSON Schema 无法编译"))?;
        validator
            .validate(value)
            .map_err(|_| invalid("configuration", "配置与 JSON Schema 不匹配"))
    }
}

/// 只跟随本文件内部的片段引用，启动扫描不能触发网络或文件访问。
fn validate_schema_references(schema: &Value, depth: usize) -> Result<(), ProtocolError> {
    if depth > 64 {
        return Err(invalid("configuration.schema", "JSON Schema 嵌套过深"));
    }
    let Some(object) = schema.as_object() else {
        return Ok(());
    };
    for key in ["$ref", "$dynamicRef", "$recursiveRef"] {
        if object.get(key).is_some_and(|reference| {
            !reference
                .as_str()
                .is_some_and(|reference| reference.starts_with('#'))
        }) {
            return Err(invalid(
                "configuration.schema",
                "只允许本地 JSON Schema 引用",
            ));
        }
    }
    if object
        .get("x-secret")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(invalid("configuration.schema", "x-secret 必须是布尔值"));
    }
    for key in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
        "dependencies",
    ] {
        if let Some(children) = object.get(key).and_then(Value::as_object) {
            for child in children.values() {
                validate_schema_references(child, depth + 1)?;
            }
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object.get(key).and_then(Value::as_array) {
            for child in children {
                validate_schema_references(child, depth + 1)?;
            }
        }
    }
    for key in [
        "items",
        "additionalItems",
        "additionalProperties",
        "unevaluatedItems",
        "unevaluatedProperties",
        "propertyNames",
        "contains",
        "not",
        "if",
        "then",
        "else",
    ] {
        if let Some(child) = object.get(key) {
            if let Some(children) = child.as_array() {
                for child in children {
                    validate_schema_references(child, depth + 1)?;
                }
            } else {
                validate_schema_references(child, depth + 1)?;
            }
        }
    }
    Ok(())
}
