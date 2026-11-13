//! 插件作者工具只依赖公开协议，避免复制宿主的清单规则。
//! @author kongweiguang

pub mod archive;
pub mod asset_url;
pub mod build;
pub mod conformance;
pub mod create;
pub mod dev;

use std::error::Error;

use kerminal_plugin_protocol::Platform;

pub type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

/// 借用协议枚举的序列化规则，避免维护与清单不一致的平台拼写。
pub fn parse_platform(value: &str) -> Result<Platform> {
    Ok(serde_json::from_value(serde_json::Value::String(
        value.to_owned(),
    ))?)
}

/// 平台标记始终采用协议序列化名称，确保资产名与安装器一致。
pub fn platform_name(platform: Platform) -> Result<String> {
    serde_json::to_value(platform)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "无法编码目标平台".into())
}
