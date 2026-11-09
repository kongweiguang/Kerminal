//! 外部插件的稳定身份，不依赖进程或文件系统。
//! @author kongweiguang

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// 无效身份不携带文件内容，避免把不可信输入带入持久化路径。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidPluginIdentity;

impl fmt::Display for InvalidPluginIdentity {
    /// 使用固定说明，具体输入留给调用方在受控上下文中诊断。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid external plugin identity")
    }
}

impl std::error::Error for InvalidPluginIdentity {}

/// 身份同时用于配置键和目录名，因此始终在构造和反序列化时验证。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PluginId(String);

impl PluginId {
    /// 固定 publisher.name 的 ASCII 语法，阻止路径分隔符与大小写别名进入存储。
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidPluginIdentity> {
        let value = value.into();
        let Some((publisher, name)) = value.split_once('.') else {
            return Err(InvalidPluginIdentity);
        };
        if !valid_part(publisher, 39) || !valid_part(name, 63) {
            return Err(InvalidPluginIdentity);
        }
        Ok(Self(value))
    }

    /// 返回验证后的原始身份，供持久键和协议消息复用。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 首字符不能为连字符，其余字符只允许小写 ASCII，保持不同平台的目录身份一致。
fn valid_part(value: &str, maximum: usize) -> bool {
    let bytes = value.as_bytes();
    (2..=maximum).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

impl fmt::Display for PluginId {
    /// 显示稳定身份而非本地化名称，避免运行时映射依赖界面语言。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PluginId {
    type Err = InvalidPluginIdentity;

    /// 字符串入口与存储入口使用同一身份规则。
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for PluginId {
    /// JSON 对象键保持人类可读的原始稳定身份。
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PluginId {
    /// 不信任磁盘文件中的键，反序列化也必须经过身份门禁。
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}
