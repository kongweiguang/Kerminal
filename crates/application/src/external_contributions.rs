//! 外部插件的冻结资源与静态贡献声明，不承担磁盘或进程生命周期。
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

use kerminal_domain::plugins::{PluginId as ExternalPluginId, PluginVersion};
use kerminal_plugin_protocol::{SettingsContribution, ValidatedPackage};
use serde_json::Value;
use thiserror::Error;

use crate::plugins::{
    ContentDescriptor, ContentTypeId, HostTypeDescriptor, HostTypeId, PluginDescriptor, PluginId,
    PluginRegistryBuilder, PluginRegistryError, ToolDescriptor, ToolId,
};

/// 启动前由后台加载器冻结资源；界面打开视图时只读内存，不进行文件操作。
#[derive(Clone, Debug)]
pub struct PreparedPlugin {
    pub id: ExternalPluginId,
    pub version: PluginVersion,
    pub directory: PathBuf,
    pub package: Arc<ValidatedPackage>,
    pub native_views: BTreeMap<String, Arc<Value>>,
    pub localizations: BTreeMap<String, BTreeMap<String, String>>,
    pub resources: BTreeMap<String, Arc<[u8]>>,
}

/// 将清单错误与注册器错误区分，调用方才能隔离具体插件并决定是否继续启动。
#[derive(Debug, Error)]
pub enum ExternalContributionError {
    #[error("插件身份或版本与安装记录不一致：{0}")]
    IdentityMismatch(String),
    #[error("插件重复：{0}")]
    DuplicatePlugin(String),
    #[error("插件与宿主 API 不兼容：{0}")]
    Incompatible(String),
    #[error("插件 API 范围无效：{0}")]
    InvalidApi(String),
    #[error("插件翻译缺失或为空：{0}")]
    InvalidTranslation(String),
    #[error(transparent)]
    Registry(#[from] PluginRegistryError),
}

/// 命令身份经过全局限定；原始声明仍可用于读取快捷键及扩展字段。
#[derive(Clone, Debug)]
pub struct ExternalCommand {
    pub id: String,
    pub title: String,
    pub declaration: kerminal_plugin_protocol::CommandContribution,
}

/// 静态注册目录之外的数据由调用方保留，避免注册器接管 UI 或命令运行时。
#[derive(Clone, Debug)]
pub struct ExternalDeclarations {
    pub provider: PluginDescriptor,
    pub tools: Vec<ToolDescriptor>,
    pub tabs: Vec<ContentDescriptor>,
    pub host_types: Vec<HostTypeDescriptor>,
    pub settings: Option<SettingsContribution>,
    pub commands: Vec<ExternalCommand>,
}

impl ExternalDeclarations {
    /// 先构造全部可能失败的描述符，确保无效清单不会部分污染注册器。
    pub fn prepare(
        prepared: &PreparedPlugin,
        host_api: &str,
        locale: &str,
    ) -> Result<Self, ExternalContributionError> {
        let manifest = &prepared.package.manifest;
        let plugin_id = prepared.id.as_str();
        if manifest.id != plugin_id || manifest.version != prepared.version.to_string() {
            return Err(ExternalContributionError::IdentityMismatch(
                plugin_id.into(),
            ));
        }
        if !manifest
            .supports_host_api(host_api)
            .map_err(|_| ExternalContributionError::InvalidApi(plugin_id.into()))?
        {
            return Err(ExternalContributionError::Incompatible(plugin_id.into()));
        }
        let provider_id = PluginId::new(plugin_id)?;
        let name = manifest
            .name
            .get(locale)
            .or_else(|| manifest.name.get("en"))
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| ExternalContributionError::InvalidTranslation(plugin_id.into()))?;
        let provider = PluginDescriptor::new(provider_id.clone(), name.clone(), true);
        let mut tools = Vec::new();
        let mut tabs = Vec::new();
        let mut host_types = Vec::new();
        let mut commands = Vec::new();
        for tool in &manifest.contributes.tools {
            let id = ToolId::new(format!("{plugin_id}/{}", tool.id))?;
            tools.push(ToolDescriptor::new(
                id,
                provider_id.clone(),
                localized_title(prepared, locale, &tool.title)?,
                None,
            ));
        }
        for tab in &manifest.contributes.tabs {
            let id = ContentTypeId::new(format!("{plugin_id}/{}", tab.id))?;
            tabs.push(ContentDescriptor::new(
                id,
                provider_id.clone(),
                localized_title(prepared, locale, &tab.title)?,
            ));
        }
        for host in &manifest.contributes.host_types {
            let id = format!("{plugin_id}/{}", host.id);
            host_types.push(HostTypeDescriptor::new(
                HostTypeId::new(&id)?,
                provider_id.clone(),
                id,
                localized_title(prepared, locale, &host.title)?,
                None,
            ));
        }
        for command in &manifest.contributes.commands {
            commands.push(ExternalCommand {
                id: format!("{plugin_id}/{}", command.id),
                title: localized_title(prepared, locale, &command.title)?,
                declaration: command.clone(),
            });
        }
        Ok(Self {
            provider,
            tools,
            tabs,
            host_types,
            settings: manifest.contributes.settings.clone(),
            commands,
        })
    }
}

/// 只解析完整翻译标记；当前语言缺失时明确回退英文，不把普通文本当表达式。
fn localized_title(
    prepared: &PreparedPlugin,
    locale: &str,
    text: &str,
) -> Result<String, ExternalContributionError> {
    let resolved = match text
        .strip_prefix('%')
        .and_then(|value| value.strip_suffix('%'))
    {
        Some(key) if !key.is_empty() => prepared
            .localizations
            .get(locale)
            .and_then(|messages| messages.get(key))
            .filter(|text| !text.trim().is_empty())
            .or_else(|| {
                prepared
                    .localizations
                    .get("en")
                    .and_then(|messages| messages.get(key))
            })
            .map(String::as_str),
        _ => Some(text),
    };
    resolved
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ExternalContributionError::InvalidTranslation(prepared.id.to_string()))
}

/// 全部插件先完成身份、兼容性和描述符检查，最后统一追加并由 freeze 校验跨来源冲突。
pub fn register_external_plugins<'a>(
    builder: &mut PluginRegistryBuilder,
    plugins: impl IntoIterator<Item = &'a PreparedPlugin>,
    host_api: &str,
    locale: &str,
) -> Result<Vec<ExternalDeclarations>, ExternalContributionError> {
    let mut identities = BTreeSet::new();
    let mut declarations = Vec::new();
    for plugin in plugins {
        if !identities.insert(plugin.id.clone()) {
            return Err(ExternalContributionError::DuplicatePlugin(
                plugin.id.to_string(),
            ));
        }
        declarations.push(ExternalDeclarations::prepare(plugin, host_api, locale)?);
    }
    for declaration in &declarations {
        builder.register_provider(declaration.provider.clone());
        for tool in &declaration.tools {
            builder.register_tool(tool.clone());
        }
        for tab in &declaration.tabs {
            builder.register_content_type(tab.clone());
        }
        for host in &declaration.host_types {
            builder.register_host_type(host.clone());
        }
    }
    Ok(declarations)
}
