//! 内置命令建议资源的窄反序列化模型。
//!
//! @author kongweiguang

use serde::Deserialize;

/// 一条静态命令定义及其来源元数据。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(super) struct CommandSpec {
    pub schema_version: u32,
    pub command: String,
    pub aliases: Vec<String>,
    pub platforms: Vec<Platform>,
    pub shells: Vec<Shell>,
    pub description: String,
    pub source: SourceSpec,
    pub tested_version: String,
    pub owner: String,
    pub updated_at: String,
    pub safety: SafetySpec,
    pub subcommands: Vec<SubcommandSpec>,
    pub options: Vec<OptionSpec>,
    pub arguments: Vec<ArgumentSpec>,
}

impl Default for CommandSpec {
    /// 缺失字段采用 schema v1 的基础值，后续版本由装载器明确拒绝。
    fn default() -> Self {
        Self {
            schema_version: 0,
            command: String::new(),
            aliases: Vec::new(),
            platforms: Vec::new(),
            shells: Vec::new(),
            description: String::new(),
            source: SourceSpec::default(),
            tested_version: String::new(),
            owner: String::new(),
            updated_at: String::new(),
            safety: SafetySpec::default(),
            subcommands: Vec::new(),
            options: Vec::new(),
            arguments: Vec::new(),
        }
    }
}

/// 来源名称和链接只用于候选来源说明，不作为执行指令。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(super) struct SourceSpec {
    pub name: String,
    pub url: String,
}

/// 资源声明的平台；远端目标没有可信平台信息时不据此猜测。
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Platform {
    Windows,
    Macos,
    Linux,
}

impl Platform {
    /// 只将本机已知的平台与本地 session 约束匹配。
    pub(super) fn is_current(self) -> bool {
        matches!(
            (self, std::env::consts::OS),
            (Self::Windows, "windows") | (Self::Macos, "macos") | (Self::Linux, "linux")
        )
    }
}

/// schema v1 支持的 shell 名称。
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Shell {
    Bash,
    Zsh,
    Fish,
    #[serde(rename = "power_shell")]
    Pwsh,
    Cmd,
}

/// 静态安全标记；敏感内容不会进入建议展示，危险项只进入主动菜单。
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(super) enum Sensitivity {
    #[default]
    Normal,
    Dangerous,
    Sensitive,
}

/// 根命令、子命令和选项共同携带的展示安全规则。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(super) struct SafetySpec {
    pub sensitivity: Sensitivity,
    pub allow_inline: bool,
    pub warning: Option<String>,
}

impl Default for SafetySpec {
    /// schema v1 未声明安全字段时按普通、可 inline 的命令处理。
    fn default() -> Self {
        Self {
            sensitivity: Sensitivity::Normal,
            allow_inline: true,
            warning: None,
        }
    }
}

/// 可选的分级子命令；path 表示其直接父级命令。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(super) struct SubcommandSpec {
    pub name: String,
    pub aliases: Vec<String>,
    pub path: Vec<String>,
    pub description: String,
    pub safety: SafetySpec,
}

impl Default for SubcommandSpec {
    /// 缺失字段保持为空，装载层会拒绝没有稳定名字的子命令。
    fn default() -> Self {
        Self {
            name: String::new(),
            aliases: Vec::new(),
            path: Vec::new(),
            description: String::new(),
            safety: SafetySpec::default(),
        }
    }
}

/// 只读选项说明与可能的参数值。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(super) struct OptionSpec {
    pub name: String,
    pub aliases: Vec<String>,
    pub path: Vec<String>,
    pub description: String,
    pub argument: Option<OptionArgumentSpec>,
    pub relationships: RelationshipSpec,
    pub safety: SafetySpec,
}

impl Default for OptionSpec {
    /// 可选字段使用 schema v1 默认项，以兼容原有效资源的省略字段。
    fn default() -> Self {
        Self {
            name: String::new(),
            aliases: Vec::new(),
            path: Vec::new(),
            description: String::new(),
            argument: None,
            relationships: RelationshipSpec::default(),
            safety: SafetySpec::default(),
        }
    }
}

/// 选项所接受的占位值；仅对静态枚举值生成补全。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(super) struct OptionArgumentSpec {
    pub placeholder: String,
    pub values: Vec<String>,
}

/// 有枚举值的命令位置参数可以参与精确补全。
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ArgumentKind {
    #[default]
    String,
    Path,
    Directory,
    File,
    Integer,
    Enum,
    Host,
    Url,
}

/// 位置参数说明；为避免猜参数位置，只使用唯一枚举参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(super) struct ArgumentSpec {
    pub name: String,
    pub path: Vec<String>,
    pub description: String,
    pub kind: ArgumentKind,
    pub required: bool,
    pub values: Vec<String>,
    pub relationships: RelationshipSpec,
    pub safety: SafetySpec,
}

impl Default for ArgumentSpec {
    /// 缺失字段适配 schema v1 默认值，不推断路径或枚举含义。
    fn default() -> Self {
        Self {
            name: String::new(),
            path: Vec::new(),
            description: String::new(),
            kind: ArgumentKind::String,
            required: false,
            values: Vec::new(),
            relationships: RelationshipSpec::default(),
            safety: SafetySpec::default(),
        }
    }
}

/// 选项关系用于避免展示与当前输入冲突的候选。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(super) struct RelationshipSpec {
    pub conflicts_with: Vec<String>,
    pub requires: Vec<String>,
}
