//! 静态插件贡献的纯数据目录；运行服务及视图实例仍由各自的组合根持有。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use thiserror::Error;

mod builtin;
pub mod capabilities;

/// 宿主插件 API 契约的权威版本；桌面启动、目录装配与迁移入口统一引用此常量。
///
/// 设计原因：该版本描述宿主向插件暴露的能力契约（1.1.0 起包含二维码、目录选择器
/// 等新贡献节点），因此声明 `>=1.1.0 <2.0.0` 的正式插件必须被旧 1.0 宿主拒绝。
/// 它与 SDK crate 自身版本以及 JSON-RPC `PROTOCOL_VERSION`（`sdk/protocol/src/runtime.rs`）
/// 是三个独立维度，禁止一起升级或互相推导。权威值放在 application 宿主侧而不是 SDK，
/// 避免旧构建把 crate 版本冒充当前宿主能力，也让 desktop 与 UI 依赖同一来源。
///
/// @author kongweiguang
pub const HOST_API_VERSION: &str = "1.1.0";

/// 核心内置单元不可关闭：本地终端、文件、编辑器与偏好是应用基本可用的前提。
///
/// @author kongweiguang
pub const CORE_PLUGIN_ID: &str = "kerminal.core";

/// 内置单元的固定声明顺序；测试与后续界面直接复用它，避免各自维护一份名单。
///
/// @author kongweiguang
const BUILTIN_UNITS: &[&str] = &[
    CORE_PLUGIN_ID,
    "kerminal.ssh",
    "kerminal.sftp",
    "kerminal.telnet",
    "kerminal.serial",
    "kerminal.rdp",
    "kerminal.agents",
    "kerminal.commands",
    "kerminal.tmux",
    "kerminal.containers",
    "kerminal.ports",
    "kerminal.system",
    "kerminal.logs",
];

/// 内置贡献沿用的兼容 provider。主机 codec 与已有主机文件都以该键标识来源，
/// 拆分只新增启停归属（owner），不改持久格式。
///
/// @author kongweiguang
pub(crate) const BUILTIN_PROVIDER_ID: &str = "kerminal";

/// 暴露固定顺序的内置单元身份，供状态映射、诊断和扩展页统一枚举。
///
/// @author kongweiguang
pub fn builtin_unit_ids() -> &'static [&'static str] {
    BUILTIN_UNITS
}

/// 判断身份是否为内置启停单元；未知值不得写入内置状态表。
///
/// @author kongweiguang
pub fn is_builtin_unit(id: &str) -> bool {
    BUILTIN_UNITS.contains(&id)
}

/// 安装器、drop-in 扫描和静态快照共用同一保留身份判定，拒绝与内置声明冲突的外部插件。
/// 兼容 provider `kerminal` 虽不作为插件出现，仍占用主机来源键，必须一并保留；
/// 集中在应用层暴露可避免 infrastructure 各处复制字面量而出现判定漂移。
///
/// @author kongweiguang
pub fn is_reserved_plugin_id(id: &str) -> bool {
    is_builtin_unit(id) || id == BUILTIN_PROVIDER_ID
}

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
    #[error("unknown plugin: {0}")]
    UnknownPlugin(String),
    #[error("plugin cannot be disabled: {0}")]
    ImmutablePlugin(String),
    #[error("contribution {contribution} is owned by missing plugin {owner}")]
    MissingOwner { contribution: String, owner: String },
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

/// 插件来源决定启停状态的取值方式：内置单元读持久状态表，外部插件的启用性
/// 由安装记录在装配时投影为 `default_enabled`，目录本身不写磁盘。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginSource {
    Builtin,
    External,
}

impl PluginSource {
    /// 启停校验需要区分内置与外部，避免把外部插件当成可写内置状态。
    ///
    /// @author kongweiguang
    pub fn is_builtin(self) -> bool {
        matches!(self, Self::Builtin)
    }
}

/// 插件来源只描述静态贡献；启停或进程执行器不由目录管理。
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PluginDescriptor {
    pub id: PluginId,
    pub name: String,
    pub default_enabled: bool,
    /// 内置单元与外部插件的启用状态来源不同，装配阶段必须能区分。
    pub source: PluginSource,
    /// 核心单元必须始终启用，扩展页和状态写入据此拒绝关闭。
    pub can_disable: bool,
}

impl PluginDescriptor {
    /// 显式指定缺省启用状态，避免未来增加描述字段时破坏第三方构造代码。
    /// 第三方插件此时只能来自外部目录，因此固定为可关闭的外部来源。
    ///
    /// @author kongweiguang
    pub fn new(id: PluginId, name: impl Into<String>, default_enabled: bool) -> Self {
        Self {
            id,
            name: name.into(),
            default_enabled,
            source: PluginSource::External,
            can_disable: true,
        }
    }

    /// 内置单元缺省全部启用；只有核心单元声明不可关闭，避免调用方另写开关表。
    ///
    /// @author kongweiguang
    pub fn builtin(id: PluginId, name: impl Into<String>, can_disable: bool) -> Self {
        Self {
            id,
            name: name.into(),
            default_enabled: true,
            source: PluginSource::Builtin,
            can_disable,
        }
    }
}

/// 主机可以引用实际工作区内容，也可以像系统 RDP 启动器一样没有内容。
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct HostTypeDescriptor {
    pub id: HostTypeId,
    /// 兼容既有主机文件的来源键；内置主机仍为 `kerminal`，单独用 owner 表达启停归属。
    pub provider: PluginId,
    /// 启停归属单元。禁用后仍保留声明供配置解析，只关闭可用入口。
    pub owner: PluginId,
    /// 内置主机沿用现有文件键；插件贡献身份仍须在全部类别中唯一。
    pub configuration_type: String,
    pub name: String,
    pub content_type: Option<ContentTypeId>,
}

impl HostTypeDescriptor {
    /// 将贡献身份与磁盘配置键分别传入，以兼容现有内置主机的持久化格式。
    /// 默认把 provider 同时当作启停归属，保持外部插件既有行为。
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
            owner: provider.clone(),
            provider,
            configuration_type: configuration_type.into(),
            name: name.into(),
            content_type,
        }
    }

    /// 内置贡献保留兼容 provider 时，用 owner 记录实际启停单元。
    ///
    /// @author kongweiguang
    pub fn owned_by(mut self, owner: PluginId) -> Self {
        self.owner = owner;
        self
    }
}

/// 工作区内容只声明身份与来源，具体 GPUI 工厂保留在 UI 侧。
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ContentDescriptor {
    pub id: ContentTypeId,
    pub provider: PluginId,
    /// 启停归属单元；禁用后声明仍保留，只从可用视图移除。
    pub owner: PluginId,
    pub name: String,
}

impl ContentDescriptor {
    /// 构造只含稳定身份的声明；跨贡献引用由冻结阶段统一校验。
    /// 默认 owner 等于 provider，使外部插件无需感知启停映射。
    ///
    /// @author kongweiguang
    pub fn new(id: ContentTypeId, provider: PluginId, name: impl Into<String>) -> Self {
        Self {
            id,
            owner: provider.clone(),
            provider,
            name: name.into(),
        }
    }

    /// 内置贡献保留 `kerminal` provider，用 owner 指向真实启停单元。
    ///
    /// @author kongweiguang
    pub fn owned_by(mut self, owner: PluginId) -> Self {
        self.owner = owner;
        self
    }
}

/// 工具面板可以关联工作区内容，但不拥有内容对应的会话或任务。
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ToolDescriptor {
    pub id: ToolId,
    pub provider: PluginId,
    /// 启停归属单元；禁用后只隐藏入口，面板偏好仍按原键保留。
    pub owner: PluginId,
    pub name: String,
    pub content_type: Option<ContentTypeId>,
}

impl ToolDescriptor {
    /// 内容关联保持可选，使只提供侧栏视图的工具无需伪造工作区内容。
    /// 默认 owner 等于 provider，外部插件因此无需显式声明归属。
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
            owner: provider.clone(),
            provider,
            name: name.into(),
            content_type,
        }
    }

    /// 内置工具保留 `kerminal` provider 以兼容既有身份，用 owner 记录启停单元。
    ///
    /// @author kongweiguang
    pub fn owned_by(mut self, owner: PluginId) -> Self {
        self.owner = owner;
        self
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
    /// 不带状态时等同全部内置单元启用，外部插件按各自的缺省值。
    ///
    /// @author kongweiguang
    pub fn freeze(self) -> Result<PluginRegistry, PluginRegistryError> {
        self.freeze_with_builtin_states(&BTreeMap::new())
    }

    /// 按持久化的内置启停状态冻结目录，只生成启用视图，不从声明集中删除禁用贡献。
    /// 主机 codec 与已有配置解析仍依赖全量声明，因此关闭插件只影响可用性查询；
    /// 未知单元、未注册单元和尝试关闭核心单元都在此拒绝，避免启动后才发现状态无法落实。
    ///
    /// @author kongweiguang
    pub fn freeze_with_builtin_states(
        self,
        builtin_states: &BTreeMap<PluginId, bool>,
    ) -> Result<PluginRegistry, PluginRegistryError> {
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
            .map(|entry| (entry.id.as_str(), entry))
            .collect::<BTreeMap<_, _>>();
        // 状态表只允许已知且已注册的内置单元，防止外部插件或拼写错误悄悄覆盖启用视图。
        for (id, enabled) in builtin_states {
            let registered_builtin = is_builtin_unit(id.as_str())
                && providers
                    .get(id.as_str())
                    .is_some_and(|entry| entry.source.is_builtin());
            if !registered_builtin {
                return Err(PluginRegistryError::UnknownPlugin(id.to_string()));
            }
            if !enabled && id.as_str() == CORE_PLUGIN_ID {
                return Err(PluginRegistryError::ImmutablePlugin(id.to_string()));
            }
        }

        let content_types = self
            .content_types
            .iter()
            .map(|entry| &entry.id)
            .collect::<BTreeSet<_>>();
        for (id, provider, owner, content) in self
            .host_types
            .iter()
            .map(|entry| {
                (
                    entry.id.as_str(),
                    &entry.provider,
                    &entry.owner,
                    entry.content_type.as_ref(),
                )
            })
            .chain(
                self.content_types
                    .iter()
                    .map(|entry| (entry.id.as_str(), &entry.provider, &entry.owner, None)),
            )
            .chain(self.tools.iter().map(|entry| {
                (
                    entry.id.as_str(),
                    &entry.provider,
                    &entry.owner,
                    entry.content_type.as_ref(),
                )
            }))
        {
            if provider.as_str() == BUILTIN_PROVIDER_ID {
                // 内置兼容 provider 不会作为插件出现；owner 才代表真实启停单元，
                // 因此只接受已注册的内置单元，避免兼容键被外部来源冒用。
                let owner_is_builtin = providers
                    .get(owner.as_str())
                    .is_some_and(|entry| entry.source.is_builtin());
                if !owner_is_builtin {
                    return Err(PluginRegistryError::MissingOwner {
                        contribution: id.to_owned(),
                        owner: owner.to_string(),
                    });
                }
            } else {
                if !providers.contains_key(provider.as_str()) {
                    return Err(PluginRegistryError::MissingProvider {
                        contribution: id.to_owned(),
                        provider: provider.to_string(),
                    });
                }
                if !providers.contains_key(owner.as_str()) {
                    return Err(PluginRegistryError::MissingOwner {
                        contribution: id.to_owned(),
                        owner: owner.to_string(),
                    });
                }
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

        let enabled = self
            .providers
            .iter()
            .filter(|entry| match entry.source {
                PluginSource::Builtin => builtin_states.get(&entry.id).copied().unwrap_or(true),
                PluginSource::External => entry.default_enabled,
            })
            .map(|entry| entry.id.clone())
            .collect();

        Ok(PluginRegistry {
            providers: self.providers,
            host_types: self.host_types,
            content_types: self.content_types,
            tools: self.tools,
            enabled,
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
    /// 冻结时的启用插件集合；声明本身全量保留，入口只查询该视图。
    enabled: BTreeSet<PluginId>,
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

    /// 插件查询与 provider 查询同义；新增命名便于启停入口表达"插件是否可用"。
    ///
    /// @author kongweiguang
    pub fn plugin(&self, id: &str) -> Option<&PluginDescriptor> {
        self.provider(id)
    }

    /// 目录中存在且处于启用视图才返回 true；未知身份一律视为不可用。
    ///
    /// @author kongweiguang
    pub fn is_enabled(&self, id: &str) -> bool {
        self.providers
            .iter()
            .any(|entry| entry.id.as_str() == id && self.enabled.contains(&entry.id))
    }

    /// 内容是否可用由归属单元决定，禁用后声明仍在，供工作区恢复识别原身份。
    ///
    /// @author kongweiguang
    pub fn content_type_enabled(&self, id: &str) -> bool {
        self.content_type(id)
            .is_some_and(|entry| self.enabled.contains(&entry.owner))
    }

    /// 工具入口随归属单元启停；面板顺序和停靠偏好由设置层单独保留。
    ///
    /// @author kongweiguang
    pub fn tool_enabled(&self, id: &str) -> bool {
        self.tool(id)
            .is_some_and(|entry| self.enabled.contains(&entry.owner))
    }

    /// 按主机配置类型判断可用性；配置键在冻结时保证全局唯一。
    ///
    /// @author kongweiguang
    pub fn host_type_enabled(&self, configuration_type: &str) -> bool {
        self.host_types
            .iter()
            .find(|entry| entry.configuration_type == configuration_type)
            .is_some_and(|entry| self.enabled.contains(&entry.owner))
    }

    /// 旧主机配置同时携带 provider 与类型，按两者定位后查询归属启用状态。
    ///
    /// @author kongweiguang
    pub fn host_type_enabled_for(&self, provider: &str, configuration_type: &str) -> bool {
        self.host_type_for_configuration(provider, configuration_type)
            .is_some_and(|entry| self.enabled.contains(&entry.owner))
    }

    /// 暴露内容归属，供门禁与诊断复用同一映射，不在调用方另写名单。
    ///
    /// @author kongweiguang
    pub fn owner_of_content_type(&self, id: &str) -> Option<&PluginId> {
        self.content_type(id).map(|entry| &entry.owner)
    }

    /// 暴露工具归属，使右栏过滤与工具查询共享冻结后的唯一答案。
    ///
    /// @author kongweiguang
    pub fn owner_of_tool(&self, id: &str) -> Option<&PluginId> {
        self.tool(id).map(|entry| &entry.owner)
    }

    /// 暴露主机归属；禁用后仍可用于解释配置为何不可连接。
    ///
    /// @author kongweiguang
    pub fn owner_of_host_type(&self, id: &str) -> Option<&PluginId> {
        self.host_type(id).map(|entry| &entry.owner)
    }
}
