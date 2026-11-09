//! 静态插件贡献的纯数据目录；运行服务及视图实例仍由各自的组合根持有。
//!
//! @author kongweiguang

use std::{collections::BTreeSet, fmt, str::FromStr};

use thiserror::Error;

mod builtin;

/// 注册失败保留具体身份，方便启动阶段定位静态装配错误。
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PluginRegistryError {
    #[error("invalid contribution identifier: {0}")]
    InvalidId(String),
    #[error("duplicate plugin or contribution identifier: {0}")]
    DuplicateId(String),
    #[error("contribution {contribution} references missing provider {provider}")]
    MissingProvider {
        contribution: String,
        provider: String,
    },
    #[error("contribution {contribution} references missing content type {content}")]
    MissingContent {
        contribution: String,
        content: String,
    },
    #[error("plugin or contribution {0} has an empty name")]
    EmptyName(String),
}

/// 兼容既有静态身份，并为外部贡献保留唯一的命名空间分隔符。
/// 提供方沿用领域插件 ID（允许尾部连字符），贡献段仍采用局部 ID 规则；
/// 命名空间前缀必须是领域插件 ID；旧内置身份仅在没有分隔符时沿用原规则。
///
/// @author kongweiguang
pub(crate) fn validate_id(value: &str) -> Result<(), PluginRegistryError> {
    let valid_segment = |segment: &str| {
        let bytes = segment.as_bytes();
        bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'-' | b'_'))
    };
    let (provider, contribution) = value
        .split_once('/')
        .map_or((value, None), |(provider, contribution)| {
            (provider, Some(contribution))
        });
    let valid_provider = if contribution.is_some() {
        kerminal_domain::plugins::PluginId::parse(provider).is_ok()
    } else {
        valid_segment(provider) || kerminal_domain::plugins::PluginId::parse(provider).is_ok()
    };
    if value.len() > 192
        || !valid_provider
        || contribution.is_some_and(|contribution| !valid_segment(contribution))
    {
        return Err(PluginRegistryError::InvalidId(value.to_owned()));
    }
    Ok(())
}

macro_rules! contribution_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            /// 仅接受稳定的 ASCII 身份，避免空白和本地化名称改变持久引用。
            ///
            /// @author kongweiguang
            pub fn new(value: impl Into<String>) -> Result<Self, PluginRegistryError> {
                let value = value.into();
                validate_id(&value)?;
                Ok(Self(value))
            }

            /// 暴露原始稳定字符串，供配置和 UI 边界显式映射。
            ///
            /// @author kongweiguang
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            /// 只借用稳定身份，不分配另一份字符串。
            ///
            /// @author kongweiguang
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            /// 日志及诊断保留原始身份，展示文案不参与身份计算。
            ///
            /// @author kongweiguang
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = PluginRegistryError;

            /// 解析与直接构造共用同一校验，防止不同入口接受不同身份。
            ///
            /// @author kongweiguang
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }
    };
}

contribution_id!(PluginId, "提供方的稳定身份，不依赖显示名称。");
contribution_id!(HostTypeId, "主机类型的稳定身份，与具体主机实例区分。");
contribution_id!(ContentTypeId, "工作区内容类型的稳定身份。");
contribution_id!(ToolId, "工具入口的稳定身份。");

/// 插件来源只描述静态贡献；启停或进程执行器不由目录管理。
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PluginDescriptor {
    pub id: PluginId,
    pub name: String,
    pub default_enabled: bool,
}

impl PluginDescriptor {
    /// 显式指定缺省启用状态，避免未来增加描述字段时破坏第三方构造代码。
    ///
    /// @author kongweiguang
    pub fn new(id: PluginId, name: impl Into<String>, default_enabled: bool) -> Self {
        Self {
            id,
            name: name.into(),
            default_enabled,
        }
    }
}

/// 主机可以引用实际工作区内容，也可以像系统 RDP 启动器一样没有内容。
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct HostTypeDescriptor {
    pub id: HostTypeId,
    pub provider: PluginId,
    /// 内置主机沿用现有文件键；插件贡献身份仍须在全部类别中唯一。
    pub configuration_type: String,
    pub name: String,
    pub content_type: Option<ContentTypeId>,
}

impl HostTypeDescriptor {
    /// 将贡献身份与磁盘配置键分别传入，以兼容现有内置主机的持久化格式。
    ///
    /// @author kongweiguang
    pub fn new(
        id: HostTypeId,
        provider: PluginId,
        configuration_type: impl Into<String>,
        name: impl Into<String>,
        content_type: Option<ContentTypeId>,
    ) -> Self {
        Self {
            id,
            provider,
            configuration_type: configuration_type.into(),
            name: name.into(),
            content_type,
        }
    }
}

/// 工作区内容只声明身份与来源，具体 GPUI 工厂保留在 UI 侧。
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ContentDescriptor {
    pub id: ContentTypeId,
    pub provider: PluginId,
    pub name: String,
}

impl ContentDescriptor {
    /// 构造只含稳定身份的声明；跨贡献引用由冻结阶段统一校验。
    ///
    /// @author kongweiguang
    pub fn new(id: ContentTypeId, provider: PluginId, name: impl Into<String>) -> Self {
        Self {
            id,
            provider,
            name: name.into(),
        }
    }
}

/// 工具面板可以关联工作区内容，但不拥有内容对应的会话或任务。
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ToolDescriptor {
    pub id: ToolId,
    pub provider: PluginId,
    pub name: String,
    pub content_type: Option<ContentTypeId>,
}

impl ToolDescriptor {
    /// 内容关联保持可选，使只提供侧栏视图的工具无需伪造工作区内容。
    ///
    /// @author kongweiguang
    pub fn new(
        id: ToolId,
        provider: PluginId,
        name: impl Into<String>,
        content_type: Option<ContentTypeId>,
    ) -> Self {
        Self {
            id,
            provider,
            name: name.into(),
            content_type,
        }
    }
}

/// 仅在启动装配阶段可变，允许贡献按任意顺序声明并统一校验引用。
#[derive(Default)]
pub struct PluginRegistryBuilder {
    providers: Vec<PluginDescriptor>,
    host_types: Vec<HostTypeDescriptor>,
    content_types: Vec<ContentDescriptor>,
    tools: Vec<ToolDescriptor>,
}

impl PluginRegistryBuilder {
    /// 返回空的装配器；冻结后由类型边界禁止继续注册。
    ///
    /// @author kongweiguang
    pub fn new() -> Self {
        Self::default()
    }

    /// 复用生产内置声明后再附加其它静态来源；最终仍需统一冻结和校验。
    ///
    /// @author kongweiguang
    pub fn register_builtins(&mut self) -> Result<&mut Self, PluginRegistryError> {
        builtin::register(self)?;
        Ok(self)
    }

    /// 记录提供方，重复及引用校验留到所有静态来源装配完成时执行。
    ///
    /// @author kongweiguang
    pub fn register_provider(&mut self, descriptor: PluginDescriptor) -> &mut Self {
        self.providers.push(descriptor);
        self
    }

    /// 记录主机类型，不接管协议服务或主机配置。
    ///
    /// @author kongweiguang
    pub fn register_host_type(&mut self, descriptor: HostTypeDescriptor) -> &mut Self {
        self.host_types.push(descriptor);
        self
    }

    /// 记录工作区内容类型，运行时工厂由上层装配。
    ///
    /// @author kongweiguang
    pub fn register_content_type(&mut self, descriptor: ContentDescriptor) -> &mut Self {
        self.content_types.push(descriptor);
        self
    }

    /// 记录工具入口，避免目录成为另一套面板或服务 owner。
    ///
    /// @author kongweiguang
    pub fn register_tool(&mut self, descriptor: ToolDescriptor) -> &mut Self {
        self.tools.push(descriptor);
        self
    }

    /// 先校验全局身份和全部引用，再消费装配器，确保配置扫描看到不可变快照。
    ///
    /// @author kongweiguang
    pub fn freeze(self) -> Result<PluginRegistry, PluginRegistryError> {
        let mut ids = BTreeSet::new();
        for (id, name) in self
            .providers
            .iter()
            .map(|entry| (entry.id.as_str(), entry.name.as_str()))
            .chain(
                self.host_types
                    .iter()
                    .map(|entry| (entry.id.as_str(), entry.name.as_str())),
            )
            .chain(
                self.content_types
                    .iter()
                    .map(|entry| (entry.id.as_str(), entry.name.as_str())),
            )
            .chain(
                self.tools
                    .iter()
                    .map(|entry| (entry.id.as_str(), entry.name.as_str())),
            )
        {
            if !ids.insert(id) {
                return Err(PluginRegistryError::DuplicateId(id.to_owned()));
            }
            if name.trim().is_empty() {
                return Err(PluginRegistryError::EmptyName(id.to_owned()));
            }
        }

        let mut host_configuration_types = BTreeSet::new();
        for entry in &self.host_types {
            validate_id(&entry.configuration_type)?;
            if !host_configuration_types.insert(entry.configuration_type.as_str()) {
                return Err(PluginRegistryError::DuplicateId(
                    entry.configuration_type.clone(),
                ));
            }
        }

        let providers = self
            .providers
            .iter()
            .map(|entry| &entry.id)
            .collect::<BTreeSet<_>>();
        let content_types = self
            .content_types
            .iter()
            .map(|entry| &entry.id)
            .collect::<BTreeSet<_>>();
        for (id, provider, content) in self
            .host_types
            .iter()
            .map(|entry| {
                (
                    entry.id.as_str(),
                    &entry.provider,
                    entry.content_type.as_ref(),
                )
            })
            .chain(
                self.content_types
                    .iter()
                    .map(|entry| (entry.id.as_str(), &entry.provider, None)),
            )
            .chain(self.tools.iter().map(|entry| {
                (
                    entry.id.as_str(),
                    &entry.provider,
                    entry.content_type.as_ref(),
                )
            }))
        {
            if !providers.contains(provider) {
                return Err(PluginRegistryError::MissingProvider {
                    contribution: id.to_owned(),
                    provider: provider.to_string(),
                });
            }
            if let Some(content) = content
                && !content_types.contains(content)
            {
                return Err(PluginRegistryError::MissingContent {
                    contribution: id.to_owned(),
                    content: content.to_string(),
                });
            }
        }

        Ok(PluginRegistry {
            providers: self.providers,
            host_types: self.host_types,
            content_types: self.content_types,
            tools: self.tools,
        })
    }
}

/// 已验证且不可修改的目录快照，不持有任何运行服务或视图。
#[derive(Clone, Debug, Default)]
pub struct PluginRegistry {
    providers: Vec<PluginDescriptor>,
    host_types: Vec<HostTypeDescriptor>,
    content_types: Vec<ContentDescriptor>,
    tools: Vec<ToolDescriptor>,
}

impl PluginRegistry {
    /// 空目录供不需要插件的测试和独立界面构造，生产桌面显式注入完整快照。
    ///
    /// @author kongweiguang
    pub fn empty() -> Self {
        Self::default()
    }

    /// 复用生产内置声明并在返回前校验；无效声明会使启动明确失败。
    ///
    /// @author kongweiguang
    pub fn builtin() -> crate::AppResult<Self> {
        let mut builder = PluginRegistryBuilder::new();
        builder.register_builtins().map_err(|source| {
            crate::AppError::infrastructure_with_source("内置插件注册失败", source)
        })?;
        builder.freeze().map_err(|source| {
            crate::AppError::infrastructure_with_source("内置插件注册失败", source)
        })
    }

    /// 按装配顺序返回已验证的静态来源。
    ///
    /// @author kongweiguang
    pub fn providers(&self) -> &[PluginDescriptor] {
        &self.providers
    }

    /// 按装配顺序返回主机描述，供配置及主机 UI 共享同一目录。
    ///
    /// @author kongweiguang
    pub fn host_types(&self) -> &[HostTypeDescriptor] {
        &self.host_types
    }

    /// 返回工作区内容描述，不暴露工厂的运行态所有权。
    ///
    /// @author kongweiguang
    pub fn content_types(&self) -> &[ContentDescriptor] {
        &self.content_types
    }

    /// 返回工具描述，不改变已有顺序或面板生命周期。
    ///
    /// @author kongweiguang
    pub fn tools(&self) -> &[ToolDescriptor] {
        &self.tools
    }

    /// 允许边界按持久字符串查找来源，未知身份不会被替换为其它插件。
    ///
    /// @author kongweiguang
    pub fn provider(&self, id: &str) -> Option<&PluginDescriptor> {
        self.providers.iter().find(|entry| entry.id.as_str() == id)
    }

    /// 配置读取按稳定身份查找主机描述，不根据显示名称推断类型。
    ///
    /// @author kongweiguang
    pub fn host_type(&self, id: &str) -> Option<&HostTypeDescriptor> {
        self.host_types.iter().find(|entry| entry.id.as_str() == id)
    }

    /// 用原主机配置类型和来源查找贡献，避免把旧磁盘键误当作跨类别身份。
    ///
    /// @author kongweiguang
    pub fn host_type_for_configuration(
        &self,
        provider: &str,
        kind: &str,
    ) -> Option<&HostTypeDescriptor> {
        self.host_types
            .iter()
            .find(|entry| entry.provider.as_str() == provider && entry.configuration_type == kind)
    }

    /// 允许内容工厂核对其贡献声明，避免渲染未知身份。
    ///
    /// @author kongweiguang
    pub fn content_type(&self, id: &str) -> Option<&ContentDescriptor> {
        self.content_types
            .iter()
            .find(|entry| entry.id.as_str() == id)
    }

    /// 允许工具工厂核对其贡献声明，避免错用其它入口。
    ///
    /// @author kongweiguang
    pub fn tool(&self, id: &str) -> Option<&ToolDescriptor> {
        self.tools.iter().find(|entry| entry.id.as_str() == id)
    }
}
