//! 桌面与离线入口共享的静态插件目录装配，不初始化运行服务。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, path::Path, sync::Arc};

use kerminal_application::{
    AppError, AppResult,
    connections::host_types::{ExternalHostCodec, HostConfigurationCatalog, HostConfigurationType},
    external_contributions::{ExternalDeclarations, PreparedPlugin, register_external_plugins},
    plugins::{
        HOST_API_VERSION, HostTypeDescriptor, HostTypeId, PluginDescriptor,
        PluginId as RegistryPluginId, PluginRegistry, PluginRegistryBuilder, is_reserved_plugin_id,
    },
};
use kerminal_infrastructure::{
    configuration::host_configuration_catalog,
    data_migration::plugins as plugin_layout,
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
///
/// 内置启停状态来自启动快照而非插件包，必须作为独立参数传入，避免目录装配
/// 自己去猜测适用哪些单元；空表表示全部内置单元启用。
///
/// @author kongweiguang
pub(crate) fn catalogs_for_plugins(
    plugins: &[Arc<PreparedPlugin>],
    active: &[Arc<PreparedPlugin>],
    locale: &str,
    builtin_states: &BTreeMap<RegistryPluginId, bool>,
) -> AppResult<PluginCatalogs> {
    catalogs_for_plugins_inner(plugins, active, locale, builtin_states, false)
}

/// 只有验收入口能附加宿主声明，所有来源仍在首次扫描前共同校验和冻结。
pub(crate) fn catalogs_for_plugins_inner(
    plugins: &[Arc<PreparedPlugin>],
    active: &[Arc<PreparedPlugin>],
    locale: &str,
    builtin_states: &BTreeMap<RegistryPluginId, bool>,
    host_list: bool,
) -> AppResult<PluginCatalogs> {
    let mut builder = PluginRegistryBuilder::new();
    builder
        .register_builtins()
        .map_err(|source| AppError::infrastructure_with_source("注册内置插件贡献失败", source))?;
    #[cfg(all(feature = "acceptance-metrics", feature = "right-rail-acceptance"))]
    if host_list {
        use kerminal_application::plugins::{
            CORE_PLUGIN_ID, ContentDescriptor, ContentTypeId, PluginId,
        };
        let content = ContentTypeId::new(kerminal_ui::RootView::HOST_LIST_PERFORMANCE_CONTENT_ID)
            .map_err(|source| {
            AppError::infrastructure_with_source("宿主列表贡献身份无效", source)
        })?;
        let provider = PluginId::new("kerminal")
            .map_err(|source| AppError::infrastructure_with_source("宿主来源身份无效", source))?;
        // 验收宿主沿用兼容 provider `kerminal`，但该键不会作为插件注册；冻结要求
        // 兼容 provider 的贡献必须有真实内置归属，因此这里显式声明由核心单元拥有。
        // 该声明只服务验收窗口，不进入生产配置或插件状态。
        let owner = PluginId::new(CORE_PLUGIN_ID)
            .map_err(|source| AppError::infrastructure_with_source("宿主归属身份无效", source))?;
        builder.register_content_type(
            ContentDescriptor::new(content, provider, "Host list acceptance").owned_by(owner),
        );
    }
    #[cfg(not(all(feature = "acceptance-metrics", feature = "right-rail-acceptance")))]
    let _ = host_list;
    register_external_plugins(
        &mut builder,
        active.iter().map(Arc::as_ref),
        // 目录装配与启动校验必须用同一宿主契约版本；引用 application 权威常量
        // 而非本地字面量，本文件同时被主程序和 kerminal-migrate 两个编译入口引入，
        // 单一来源才能保证迁移入口与桌面看到的插件兼容判定一致。
        HOST_API_VERSION,
        locale,
    )
    .map_err(|source| AppError::infrastructure_with_source("注册外部插件贡献失败", source))?;
    for plugin in plugins
        .iter()
        .filter(|plugin| !active.iter().any(|enabled| enabled.id == plugin.id))
    {
        let mut declaration = ExternalDeclarations::prepare(plugin, HOST_API_VERSION, locale)
            .map_err(|source| {
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
            .freeze_with_builtin_states(builtin_states)
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

/// 迁移前只读冻结目录：schema 2 走静态快照，schema 1 只读 active 包与 host schema。
///
/// 桌面启动顺序要求迁移服务先拿到主机 codec；旧布局不能让普通启动先写插件状态，
/// 因此这里只读取、不应用 pending、不移动目录，未知布局退回内置目录让预检报告阻断。
pub(crate) fn migration_catalogs(root: &Path) -> AppResult<PluginCatalogs> {
    let absolute = absolute_root(root)?;
    let inspection = plugin_layout::inspect(&absolute)?;
    // 迁移 journal 存在时旧状态可能已移入 backup 或 schema 2 尚未写出；journal 保存了
    // 完整声明与来源，必须优先提供旧 host codec，否则恢复末段会因无法解析旧主机再次失败。
    // 读取失败保持原错误，不能退回内置目录掩盖证据。
    if inspection.migration_pending {
        return legacy_catalogs(&absolute);
    }
    match inspection.class {
        plugin_layout::PluginLayoutClass::Fresh => builtin_catalogs(),
        plugin_layout::PluginLayoutClass::Current => offline_catalogs(&absolute),
        plugin_layout::PluginLayoutClass::Schema1Legacy
        | plugin_layout::PluginLayoutClass::Schema1Empty => legacy_catalogs(&absolute),
        // 预检本身会返回固定 blocker；先用内置 codec 让 CLI 能把阻断结果完整输出。
        plugin_layout::PluginLayoutClass::Residue | plugin_layout::PluginLayoutClass::Recovery => {
            builtin_catalogs()
        }
    }
}

/// 旧包只贡献迁移所需的主机 codec；不注册工具、视图或运行能力，避免在迁移前执行插件声明。
fn legacy_catalogs(root: &Path) -> AppResult<PluginCatalogs> {
    let Some(declarations) = plugin_layout::legacy_declarations(root)? else {
        return builtin_catalogs();
    };
    let mut builder = PluginRegistryBuilder::new();
    builder
        .register_builtins()
        .map_err(|source| AppError::infrastructure_with_source("注册内置插件贡献失败", source))?;
    let mut types = Vec::new();
    for declaration in declarations {
        // 保留身份只能由内置单元拥有：旧数据把 `kerminal.*` 当作普通插件声明时，
        // 再注册 provider/host codec 会与内置声明重复并使冻结失败。内置 codec 已覆盖
        // 这些协议，因此只跳过外部注册，旧目录与原始声明保持不动以便迁移审计。
        if is_reserved_plugin_id(declaration.id.as_str()) {
            tracing::warn!(
                event = "LEGACY_PLUGIN_RESERVED_ID_SKIPPED",
                plugin = declaration.id.as_str(),
            );
            continue;
        }
        let provider = RegistryPluginId::new(declaration.id.as_str()).map_err(|source| {
            AppError::infrastructure_with_source("旧插件提供方身份无效", source)
        })?;
        builder.register_provider(PluginDescriptor::new(
            provider.clone(),
            declaration.name.clone(),
            declaration.enabled,
        ));
        for host in &declaration.host_types {
            let host_type = HostTypeId::new(&host.configuration_type).map_err(|source| {
                AppError::infrastructure_with_source("旧插件主机身份无效", source)
            })?;
            builder.register_host_type(HostTypeDescriptor::new(
                host_type,
                provider.clone(),
                host.configuration_type.clone(),
                host.title.clone(),
                None,
            ));
            types.push(HostConfigurationType::new(
                declaration.id.as_str(),
                host.configuration_type.clone(),
                Arc::new(ExternalHostCodec::new(
                    host.schema.clone(),
                    declaration.id.as_str(),
                )?),
            )?);
        }
    }
    let registry =
        Arc::new(builder.freeze().map_err(|source| {
            AppError::infrastructure_with_source("冻结旧插件目录失败", source)
        })?);
    let hosts = host_configuration_catalog(types)?;
    validate_host_catalog(&registry, &hosts)?;
    Ok((registry, hosts))
}

/// 相对根按调用进程当前目录解释；只做路径解析，不创建目录或锁。
fn absolute_root(root: &Path) -> AppResult<std::path::PathBuf> {
    if root.is_absolute() {
        Ok(root.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .map_err(|source| AppError::infrastructure_with_source("解析配置根失败", source))?
            .join(root))
    }
}

/// 只读取当前生效包并复核状态；相对路径按调用进程当前目录解释，不创建目录。
pub(crate) fn offline_catalogs(root: &Path) -> AppResult<PluginCatalogs> {
    let absolute = absolute_root(root)?;
    let store = PluginStore::new(StorageCoordinator::new(absolute))
        .map_err(|source| AppError::infrastructure_with_source("读取插件存储失败", source))?;
    let platform = kerminal_plugin_protocol::Platform::current()
        .ok_or_else(|| AppError::unsupported("当前平台不支持独立插件"))?;
    let (plugins, _) = store
        .read_static_plugins(&ProtocolBootValidator {
            // 离线读取沿用与在线启动相同的权威宿主契约；声明 `>=1.1.0` 的新插件
            // 在离线目录同样可见，旧 1.0 字面量会让同一插件在两个入口出现判定漂移。
            host_api: HOST_API_VERSION.into(),
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
        // 离线入口只读取已生效包，不解析持久内置状态；全启用与迁移前目录一致。
        &BTreeMap::new(),
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

#[cfg(test)]
#[path = "../tests/support/plugin_catalogs_unit.rs"]
mod tests;
