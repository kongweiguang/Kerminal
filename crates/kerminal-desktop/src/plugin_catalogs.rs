//! 桌面与离线入口共享的静态插件目录装配，不初始化运行服务。
//!
//! @author kongweiguang

use std::{path::Path, sync::Arc};

use kerminal_application::{
    AppError, AppResult,
    connections::host_types::{ExternalHostCodec, HostConfigurationCatalog, HostConfigurationType},
    external_contributions::{ExternalDeclarations, PreparedPlugin, register_external_plugins},
    plugins::{PluginRegistry, PluginRegistryBuilder},
};
use kerminal_infrastructure::{
    configuration::host_configuration_catalog,
    plugins::{boot::ProtocolBootValidator, store::PluginStore},
    storage::StorageCoordinator,
};

pub(crate) type PluginCatalogs = (Arc<PluginRegistry>, Arc<HostConfigurationCatalog>);

/// 首次配置扫描前冻结声明和 codec，确保迁移、校验与桌面服务使用相同规则。
///
/// @author kongweiguang
pub(crate) fn builtin_catalogs() -> AppResult<PluginCatalogs> {
    let plugins = Arc::new(PluginRegistry::builtin()?);
    let hosts = host_configuration_catalog(Vec::new())?;
    validate_host_catalog(&plugins, &hosts)?;
    Ok((plugins, hosts))
}

/// 清单和 codec 同时冻结，禁用插件保留静态配置能力但不创建运行服务。
pub(crate) fn catalogs_for_plugins(
    plugins: &[Arc<PreparedPlugin>],
    active: &[Arc<PreparedPlugin>],
    locale: &str,
) -> AppResult<PluginCatalogs> {
    let mut builder = PluginRegistryBuilder::new();
    builder
        .register_builtins()
        .map_err(|source| AppError::infrastructure_with_source("注册内置插件贡献失败", source))?;
    register_external_plugins(
        &mut builder,
        active.iter().map(Arc::as_ref),
        "1.0.0",
        locale,
    )
    .map_err(|source| AppError::infrastructure_with_source("注册外部插件贡献失败", source))?;
    for plugin in plugins
        .iter()
        .filter(|plugin| !active.iter().any(|enabled| enabled.id == plugin.id))
    {
        let mut declaration =
            ExternalDeclarations::prepare(plugin, "1.0.0", locale).map_err(|source| {
                AppError::infrastructure_with_source("读取静态插件声明失败", source)
            })?;
        declaration.provider.default_enabled = false;
        builder.register_provider(declaration.provider);
        for host in declaration.host_types {
            builder.register_host_type(host);
        }
    }
    let registry = Arc::new(
        builder
            .freeze()
            .map_err(|source| AppError::infrastructure_with_source("冻结插件贡献失败", source))?,
    );
    let mut types = Vec::new();
    for plugin in plugins {
        for host in &plugin.package.manifest.contributes.host_types {
            let schema = plugin
                .package
                .configuration_schemas
                .get(&host.schema)
                .ok_or_else(|| AppError::invalid("插件主机 schema 缺失"))?;
            types.push(HostConfigurationType::new(
                plugin.id.as_str(),
                format!("{}/{}", plugin.id.as_str(), host.id),
                Arc::new(ExternalHostCodec::new(schema.clone(), plugin.id.as_str())?),
            )?);
        }
    }
    let hosts = host_configuration_catalog(types)?;
    validate_host_catalog(&registry, &hosts)?;
    Ok((registry, hosts))
}

/// 只读取当前生效包并复核状态；相对路径按调用进程当前目录解释，不创建目录。
pub(crate) fn offline_catalogs(root: &Path) -> AppResult<PluginCatalogs> {
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| AppError::infrastructure_with_source("解析配置根失败", source))?
            .join(root)
    };
    let store = PluginStore::new(StorageCoordinator::new(absolute))
        .map_err(|source| AppError::infrastructure_with_source("读取插件存储失败", source))?;
    let platform = kerminal_plugin_protocol::Platform::current()
        .ok_or_else(|| AppError::unsupported("当前平台不支持独立插件"))?;
    let (plugins, _) = store
        .read_static_plugins(&ProtocolBootValidator {
            host_api: "1.0.0".into(),
            platform,
        })
        .map_err(|source| AppError::infrastructure_with_source("读取插件静态快照失败", source))?;
    if plugins.is_empty() {
        return builtin_catalogs();
    }
    catalogs_for_plugins(
        &plugins
            .into_iter()
            .map(|plugin| Arc::new(plugin.manifest))
            .collect::<Vec<_>>(),
        &[],
        "en",
    )
}

/// 只接受双向对应的声明和 codec，避免扫描后才发现孤立或错误提供方。
///
/// @author kongweiguang
pub(crate) fn validate_host_catalog(
    plugins: &PluginRegistry,
    hosts: &HostConfigurationCatalog,
) -> AppResult<()> {
    for entry in plugins.host_types() {
        if hosts
            .get(entry.provider.as_str(), &entry.configuration_type)
            .is_none()
        {
            return Err(AppError::invalid("主机插件声明缺少对应配置 codec"));
        }
    }
    for entry in hosts.iter() {
        if plugins
            .host_type_for_configuration(&entry.provider_id, &entry.type_id)
            .is_none()
        {
            return Err(AppError::invalid("主机配置 codec 缺少对应插件声明"));
        }
    }
    Ok(())
}
