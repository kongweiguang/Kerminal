//! 外部插件声明与线协议的唯一来源，不依赖宿主或界面实现。
//! @author kongweiguang

pub mod asset_url;
pub mod configuration;
pub mod manifest;
pub mod native;
pub mod package;
pub mod runtime;
pub mod schema;

pub use asset_url::*;
pub use configuration::*;
pub use manifest::*;
pub use native::*;
pub use package::*;
pub use runtime::*;

use std::collections::BTreeMap;

use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

/// 只暴露错误位置和规则，避免把用户数据或插件密钥写入日志。
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("{location}: {reason}")]
    Invalid { location: String, reason: String },
    #[error("{location}: JSON 格式无效: {source}")]
    Json {
        location: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("{location}: 文件访问失败: {source}")]
    Io {
        location: String,
        #[source]
        source: std::io::Error,
    },
}

/// 统一构造带位置的校验错误，调用方无需解析实际插件内容。
pub(crate) fn invalid(location: impl Into<String>, reason: impl Into<String>) -> ProtocolError {
    ProtocolError::Invalid {
        location: location.into(),
        reason: reason.into(),
    }
}

/// 拒绝重复 JSON 键，避免安装器、宿主和 SDK 对同一字节作不同解释。
pub(crate) fn parse_json(bytes: &[u8], location: &str) -> Result<Value, ProtocolError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = StrictValue
        .deserialize(&mut deserializer)
        .map_err(|source| ProtocolError::Json {
            location: location.into(),
            source,
        })?;
    deserializer.end().map_err(|source| ProtocolError::Json {
        location: location.into(),
        source,
    })?;
    Ok(value)
}

struct StrictValue;

impl<'de> DeserializeSeed<'de> for StrictValue {
    type Value = Value;

    /// 嵌套对象也复用同一访问器，确保重复字段无法藏在子对象中。
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(StrictVisitor)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    /// 错误只说明格式要求，不回显可能包含秘密的值。
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("没有重复键的 JSON 值")
    }

    /// 保留 JSON 布尔值而不改变其语义。
    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(value.into())
    }
    /// 保留 JSON 有符号整数的范围。
    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(value.into())
    }
    /// 保留 JSON 无符号整数的范围。
    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(value.into())
    }
    /// 只接受 JSON 可以表达的有限浮点数。
    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("无效数字"))
    }
    /// 保留文本供后续契约校验，不在解析阶段重写文案。
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(value.into())
    }
    /// 空值必须与缺少字段区分。
    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    /// 所有数组元素继续递归检查对象键。
    fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = access.next_element_seed(StrictValue)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    /// 出现同名成员时立即拒绝，不能采用最后一个值覆盖。
    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut values = Map::new();
        while let Some(key) = access.next_key::<String>()? {
            let value = access.next_value_seed(StrictValue)?;
            if values.insert(key, value).is_some() {
                return Err(serde::de::Error::custom("JSON 对象存在重复字段"));
            }
        }
        Ok(Value::Object(values))
    }
}

/// 类型化对象只接受预留的 x- 扩展；扩展值原样保留供往返使用。
pub(crate) fn validate_extensions(
    extensions: &BTreeMap<String, Value>,
    location: &str,
) -> Result<(), ProtocolError> {
    for key in extensions.keys() {
        if !key.starts_with("x-") || key.len() <= 2 {
            return Err(invalid(location, format!("未知字段 {key}")));
        }
    }
    Ok(())
}
