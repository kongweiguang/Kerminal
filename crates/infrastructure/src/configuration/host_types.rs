//! 内置主机 codec 与扩展 codec 在同一不可变目录中装配。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_application::{
    AppError, AppResult,
    connections::host_types::{
        HostConfigurationCatalog, HostConfigurationCodec, HostConfigurationType,
    },
};

const BUILTIN_TYPES: [&str; 6] = ["ssh", "sftp", "rdp", "telnet", "serial", "local"];

/// 在任何配置读取前收集全部实现；重复的类型标识不能覆盖内置类型。
pub fn host_configuration_catalog(
    extensions: Vec<HostConfigurationType>,
) -> AppResult<Arc<HostConfigurationCatalog>> {
    let mut types = Vec::with_capacity(BUILTIN_TYPES.len() + extensions.len());
    for type_id in BUILTIN_TYPES {
        types.push(HostConfigurationType::new(
            "kerminal",
            type_id,
            Arc::new(BuiltinHostCodec { type_id }),
        )?);
    }
    types.extend(extensions);
    Ok(Arc::new(HostConfigurationCatalog::new(types)?))
}

/// 只校验内置 codec 及协议绑定，允许运行提供方与既有文件格式独立装配。
pub(crate) fn ensure_builtin_types(catalog: &HostConfigurationCatalog) -> AppResult<()> {
    for type_id in BUILTIN_TYPES {
        let entry = catalog
            .get("kerminal", type_id)
            .ok_or_else(|| AppError::invalid("主机配置目录缺少内置类型"))?;
        let codec: &dyn std::any::Any = entry.codec.as_ref();
        if !codec
            .downcast_ref::<BuiltinHostCodec>()
            .is_some_and(|codec| codec.type_id == type_id)
        {
            return Err(AppError::invalid("内置主机 codec 不匹配"));
        }
    }
    if catalog.iter().any(|entry| {
        entry.provider_id == "kerminal" && !BUILTIN_TYPES.contains(&entry.type_id.as_str())
    }) {
        return Err(AppError::invalid("主机配置目录含未知内置类型"));
    }
    Ok(())
}

struct BuiltinHostCodec {
    type_id: &'static str,
}

impl HostConfigurationCodec for BuiltinHostCodec {
    /// 内置类型沿用 schema v3 校验，不改变现有六类文件的磁盘格式。
    fn decode(&self, version: u32, configuration: &toml::Value) -> AppResult<toml::Value> {
        if version != 3 {
            return Err(AppError::invalid("内置主机配置版本不受支持"));
        }
        super::validation::validate_builtin_host_configuration(self.type_id, configuration)
            .map_err(|source| {
                AppError::configuration_corrupt("内置主机配置").with_source(source)
            })?;
        Ok(configuration.clone())
    }

    /// 内置写入仍由现有类型化 adapter 生成；编码后重新验证以共用同一 schema。
    fn encode(&self, version: u32, configuration: &toml::Value) -> AppResult<toml::Value> {
        self.decode(version, configuration)
    }
}
