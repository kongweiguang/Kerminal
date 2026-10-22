//! 不同领域上下文共享的不透明标识。
//!
//! @author kongweiguang

use std::fmt::{Display, Formatter};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::DomainError;

macro_rules! opaque_identity {
    ($name:ident) => {
        /// An opaque stable identifier that is safe to use as one storage path component.
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            /// 新实体使用 UUID 生成稳定标识；旧标识读取不绑定 UUID 格式，保留磁盘兼容性。
            pub fn new() -> Self {
                Self(Uuid::new_v4().to_string())
            }

            /// 解析持久化的不透明标识，只拒绝空值与控制字符；路径安全由存储适配器编码负责。
            pub fn parse(value: &str) -> Result<Self, DomainError> {
                if value.is_empty() {
                    return Err(DomainError::EmptyIdentity);
                }
                if value.chars().any(char::is_control) {
                    return Err(DomainError::UnsafeIdentity);
                }
                Ok(Self(value.to_owned()))
            }

            /// 返回原样持久化标识，避免在 UI、MCP 与仓储边界重复归一化。
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            /// 新实体默认创建新标识，不能用固定空值表示尚未分配的 ID。
            fn default() -> Self {
                Self::new()
            }
        }

        impl Display for $name {
            /// 仅格式化原始标识；调用方应避免把其直接拼入文件系统路径。
            fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl Serialize for $name {
            /// 继续以历史纯字符串形态序列化，兼容 workspace v3 等现有 JSON 数据。
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            /// 反序列化时重新执行轻量不变量校验，但不猜测旧标识的格式或语义。
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(&value).map_err(serde::de::Error::custom)
            }
        }
    };
}

opaque_identity!(HostId);
opaque_identity!(SessionId);
opaque_identity!(PaneId);
opaque_identity!(TabId);
opaque_identity!(AgentSessionId);
opaque_identity!(TransferId);
