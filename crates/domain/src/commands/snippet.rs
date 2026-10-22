//! 用户片段定义、模板变量和安全元数据约束。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::connections::RemoteShellDialect;

/// 片段面向的终端范围。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SnippetScope {
    #[default]
    Any,
    Local,
    Ssh,
}

/// 片段在目录中的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SnippetCatalogOrigin {
    User,
    Builtin,
}

/// 片段偏好按来源和稳定 ID 隔离，避免用户片段与内置片段互相覆盖。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetPreferenceKey {
    pub origin: SnippetCatalogOrigin,
    pub snippet_id: String,
}

/// 收藏和使用历史只记录用户实际完成的动作类型，不包含渲染后的命令正文。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SnippetUsageAction {
    Insert,
    Run,
    CopyRendered,
}

/// 目录偏好投影与片段定义分离，统计缺失不应改变模板内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetPreference {
    pub key: SnippetPreferenceKey,
    pub favorite: bool,
    pub use_count: u64,
    pub last_action: Option<SnippetUsageAction>,
    pub last_used_at_unix_ms: Option<i64>,
}

/// 一次用户确认完成的片段动作使用稳定 receipt，取消后的重试不会重复累计。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetUsageRecordRequest {
    pub receipt_id: String,
    pub key: SnippetPreferenceKey,
    pub action: SnippetUsageAction,
}

/// 已提交的使用记录回执不携带敏感变量或展开后的命令文本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetUsageReceipt {
    pub receipt_id: String,
    pub key: SnippetPreferenceKey,
    pub action: SnippetUsageAction,
    pub occurred_at_unix_ms: i64,
}

/// 清理使用轨迹时保留用户收藏，并分别返回删除的回执数和重置的偏好数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetUsageClearResult {
    pub receipts_removed: u64,
    pub preferences_reset: u64,
}

/// 风险级别由片段元数据显式给出，不从标题、标签或主机名推断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SnippetRisk {
    Inspect,
    Change,
    Destructive,
    Unknown,
}

/// 插入或执行的默认交互。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SnippetDefaultAction {
    Insert,
    Run,
}

/// 用户可见上下文绑定类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SnippetContextBindingKind {
    Global,
    Workspace,
    Host,
    HostGroup,
}

/// 片段可用范围绑定；非 global 必须指定目标。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetContextBinding {
    pub kind: SnippetContextBindingKind,
    pub target_id: Option<String>,
}

/// 支持的变量类型；解析模板时不接受任意未知类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SnippetVariableKind {
    Text,
    Path,
    Port,
    Integer,
    Host,
    Url,
    Service,
    Container,
    Enum,
    Secret,
    Raw,
}

/// 变量写入 shell 命令时采用的转义策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SnippetVariableRenderStrategy {
    ShellArg,
    ValidatedRaw,
    Literal,
}

/// 模板变量；敏感变量不能把默认值或建议值持久化在目录文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetVariable {
    pub name: String,
    pub label: String,
    pub description: String,
    pub kind: SnippetVariableKind,
    pub required: bool,
    pub default_value: Option<String>,
    pub suggestions: Vec<String>,
    pub validation: Option<String>,
    pub render_strategy: SnippetVariableRenderStrategy,
    pub sensitive: bool,
}

/// 可写入 `.kerminal/snippets` 的用户维护定义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetDefinition {
    pub id: String,
    pub title: String,
    pub command: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    /// 目录顺序由用户维护；编辑正文时应和定义一起通过版本化端口往返。
    pub sort_order: i64,
    pub scope: SnippetScope,
    pub category: Option<String>,
    pub risk: SnippetRisk,
    pub default_action: SnippetDefaultAction,
    pub variables: Vec<SnippetVariable>,
    pub context_bindings: Vec<SnippetContextBinding>,
    pub derived_from: Option<String>,
}

impl SnippetDefinition {
    /// 在保存文件前一次性校验字段、变量策略和模板占位符，避免运行时猜测。
    pub fn validate(&self) -> Result<(), SnippetValidationError> {
        if self.title.trim().is_empty() {
            return Err(SnippetValidationError::EmptyTitle);
        }
        if self.title.chars().count() > 80 {
            return Err(SnippetValidationError::TitleTooLong);
        }
        if self.command.trim().is_empty() {
            return Err(SnippetValidationError::EmptyCommand);
        }
        if self.command.chars().count() > 8_000 {
            return Err(SnippetValidationError::CommandTooLong);
        }
        if self.tags.len() > 12 || self.tags.iter().any(|tag| tag.chars().count() > 32) {
            return Err(SnippetValidationError::InvalidTags);
        }
        if self.variables.len() > 32 {
            return Err(SnippetValidationError::TooManyVariables);
        }
        let mut names = HashSet::with_capacity(self.variables.len());
        for variable in &self.variables {
            if !valid_variable_name(&variable.name) || variable.label.trim().is_empty() {
                return Err(SnippetValidationError::InvalidVariableName);
            }
            if !names.insert(variable.name.as_str()) {
                return Err(SnippetValidationError::DuplicateVariable);
            }
            if (variable.kind == SnippetVariableKind::Secret || variable.sensitive)
                && (variable
                    .default_value
                    .as_deref()
                    .is_some_and(|value| !value.is_empty())
                    || !variable.suggestions.is_empty())
            {
                return Err(SnippetValidationError::SensitiveDefaultsForbidden);
            }
            if variable.render_strategy == SnippetVariableRenderStrategy::ValidatedRaw
                && !matches!(
                    variable.kind,
                    SnippetVariableKind::Port
                        | SnippetVariableKind::Integer
                        | SnippetVariableKind::Host
                        | SnippetVariableKind::Service
                        | SnippetVariableKind::Container
                        | SnippetVariableKind::Enum
                )
            {
                return Err(SnippetValidationError::UnsafeRawVariable);
            }
            if variable
                .validation
                .as_deref()
                .is_some_and(invalid_validation_pattern)
            {
                return Err(SnippetValidationError::InvalidValidationPattern);
            }
        }
        if self.context_bindings.iter().any(|binding| {
            binding.kind != SnippetContextBindingKind::Global
                && binding
                    .target_id
                    .as_deref()
                    .is_none_or(|target| target.trim().is_empty())
        }) {
            return Err(SnippetValidationError::MissingContextTarget);
        }
        if !placeholders_are_declared(&self.command, &names) {
            return Err(SnippetValidationError::UndeclaredPlaceholder);
        }
        Ok(())
    }
}

/// 目录展示投影。统计和收藏不改变片段模板内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetCatalogItem {
    pub id: String,
    pub origin: SnippetCatalogOrigin,
    pub title: String,
    pub description: String,
    pub template: String,
    pub category: String,
    pub pack: String,
    pub tags: Vec<String>,
    pub scope: SnippetScope,
    pub platforms: Vec<String>,
    pub shells: Vec<String>,
    pub capabilities: Vec<String>,
    pub risk: SnippetRisk,
    pub sensitive: bool,
    pub duration: String,
    pub default_action: SnippetDefaultAction,
    pub variables: Vec<SnippetVariable>,
    pub context_bindings: Vec<SnippetContextBinding>,
    /// 用户片段从内置模板派生时保留的源身份，供 JSON v1 导出往返。
    pub derived_from: Option<String>,
    pub catalog_version: Option<String>,
    /// 来自冻结 V2 目录的命令规范标识；用户片段没有该字段。
    pub command_spec: Option<String>,
    /// 内置片段的维护方元数据；不据此推断信任或执行权限。
    pub owner: Option<String>,
    /// 内置片段声明的验证版本，仅用于目录信息展示。
    pub tested_version: Option<String>,
    pub source_name: Option<String>,
    pub source_url: Option<String>,
    pub deprecated: bool,
    pub favorite: bool,
    pub use_count: u64,
    pub last_used_at_unix_ms: Option<i64>,
    pub sort_order: i64,
    pub updated_at: String,
}

/// 片段运行适用性规则的粗粒度结果；平台未被可信运行时报告时不能标成兼容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SnippetApplicabilityStatus {
    Unrestricted,
    Compatible,
    Unknown,
    Incompatible,
}

/// 适用性结果只说明当前可证明的约束，不把缺少平台或精确 shell 名称当成匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SnippetApplicabilityReason {
    TargetNotAvailable,
    PlatformNotReported,
    ExactShellNotReported,
    ShellDialectMismatch,
}

/// 供应用和 UI 共用的纯适用性判断结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetApplicabilityAssessment {
    pub status: SnippetApplicabilityStatus,
    pub reason: Option<SnippetApplicabilityReason>,
}

/// 依目录声明和可信运行时方言判断内置片段是否明确不兼容。
///
/// POSIX 只代表一类转义协议，不能据此断定具体是 Bash 或 Zsh；平台也必须来自与当前会话绑定的可信来源，
/// 因此无法证明的部分返回 Unknown。用户片段不会继承内置目录的限制。
///
/// @author kongweiguang
pub fn assess_snippet_applicability(
    origin: SnippetCatalogOrigin,
    platforms: &[String],
    shells: &[String],
    shell_dialect: Option<RemoteShellDialect>,
) -> SnippetApplicabilityAssessment {
    if origin == SnippetCatalogOrigin::User || (platforms.is_empty() && shells.is_empty()) {
        return SnippetApplicabilityAssessment {
            status: SnippetApplicabilityStatus::Unrestricted,
            reason: None,
        };
    }

    if !shells.is_empty() {
        let Some(dialect) = shell_dialect else {
            return unknown_applicability(SnippetApplicabilityReason::TargetNotAvailable);
        };
        match assess_shell_dialect(shells, dialect) {
            ShellApplicability::Incompatible => {
                return SnippetApplicabilityAssessment {
                    status: SnippetApplicabilityStatus::Incompatible,
                    reason: Some(SnippetApplicabilityReason::ShellDialectMismatch),
                };
            }
            ShellApplicability::Unknown => {
                return unknown_applicability(SnippetApplicabilityReason::ExactShellNotReported);
            }
            ShellApplicability::Compatible => {}
        }
    }

    if !platforms.is_empty() {
        return unknown_applicability(SnippetApplicabilityReason::PlatformNotReported);
    }

    SnippetApplicabilityAssessment {
        status: SnippetApplicabilityStatus::Compatible,
        reason: None,
    }
}

/// 把可信 shell dialect 与声明的 shell 家族比较；POSIX 无法区分 Bash/Zsh，未知声明也不足以证明冲突。
///
/// @author kongweiguang
fn assess_shell_dialect(shells: &[String], dialect: RemoteShellDialect) -> ShellApplicability {
    let declared = shells
        .iter()
        .map(|shell| declared_shell_family(shell))
        .collect::<Vec<_>>();
    let has_unknown_name = declared.iter().any(Option::is_none);
    let families = declared.into_iter().flatten().collect::<Vec<_>>();

    match dialect {
        RemoteShellDialect::Unknown => ShellApplicability::Unknown,
        RemoteShellDialect::Posix => {
            if families.contains(&DeclaredShellFamily::Posix) || has_unknown_name {
                ShellApplicability::Unknown
            } else {
                ShellApplicability::Incompatible
            }
        }
        RemoteShellDialect::Fish => {
            assess_named_shell_family(&families, has_unknown_name, DeclaredShellFamily::Fish)
        }
        RemoteShellDialect::PowerShell => {
            assess_named_shell_family(&families, has_unknown_name, DeclaredShellFamily::PowerShell)
        }
    }
}

/// 只有目录中明确声明当前 shell 家族时才返回匹配；未知名称阻止否定式兼容判断。
fn assess_named_shell_family(
    families: &[DeclaredShellFamily],
    has_unknown_name: bool,
    current: DeclaredShellFamily,
) -> ShellApplicability {
    if families.contains(&current) {
        ShellApplicability::Compatible
    } else if has_unknown_name {
        ShellApplicability::Unknown
    } else {
        ShellApplicability::Incompatible
    }
}

/// 归一化冻结目录中的 shell 名称；无法识别的标签保留为 Unknown，而不是猜测其语义。
fn declared_shell_family(shell: &str) -> Option<DeclaredShellFamily> {
    let normalized = shell
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    match normalized.as_str() {
        "bash" | "zsh" | "sh" => Some(DeclaredShellFamily::Posix),
        "fish" => Some(DeclaredShellFamily::Fish),
        "powershell" | "powershellcore" | "pwsh" => Some(DeclaredShellFamily::PowerShell),
        "cmd" | "cmdexe" => Some(DeclaredShellFamily::WindowsCommand),
        _ => None,
    }
}

/// 构造显式 Unknown，统一保留无法核实的原因供 UI 展示。
fn unknown_applicability(reason: SnippetApplicabilityReason) -> SnippetApplicabilityAssessment {
    SnippetApplicabilityAssessment {
        status: SnippetApplicabilityStatus::Unknown,
        reason: Some(reason),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeclaredShellFamily {
    Posix,
    Fish,
    PowerShell,
    WindowsCommand,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShellApplicability {
    Compatible,
    Unknown,
    Incompatible,
}

/// 列出用户或内置片段时使用的查询条件。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetCatalogListRequest {
    pub query: Option<String>,
    pub origin: Option<SnippetCatalogOrigin>,
    pub scope: Option<SnippetScope>,
    pub limit: Option<usize>,
}

/// 片段文件在进入目录前的领域校验错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnippetValidationError {
    EmptyTitle,
    TitleTooLong,
    EmptyCommand,
    CommandTooLong,
    InvalidTags,
    InvalidVariableName,
    DuplicateVariable,
    SensitiveDefaultsForbidden,
    UnsafeRawVariable,
    InvalidValidationPattern,
    MissingContextTarget,
    UndeclaredPlaceholder,
    TooManyVariables,
}

/// 限制变量名为模板标识符，防止占位符语法被 shell 片段或路径分隔符混淆。
fn valid_variable_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

/// 确认每个双花括号占位符都闭合且引用已声明变量，避免保存后运行时才发现模板残缺。
fn placeholders_are_declared(command: &str, declared: &HashSet<&str>) -> bool {
    let mut remaining = command;
    loop {
        let Some(open) = remaining.find("{{") else {
            return !remaining.contains("}}");
        };
        if remaining[..open].contains("}}") {
            return false;
        }
        let after_open = &remaining[open + 2..];
        let Some(close) = after_open.find("}}") else {
            return false;
        };
        let name = &after_open[..close];
        if !valid_variable_name(name) || !declared.contains(name) {
            return false;
        }
        remaining = &after_open[close + 2..];
    }
}

/// 限制校验表达式能力，避免配置文件引入高风险回溯特性。
fn invalid_validation_pattern(pattern: &str) -> bool {
    pattern.len() > 128
        || pattern
            .chars()
            .any(|value| matches!(value, '(' | ')' | '{' | '}' | '|' | '\r' | '\n'))
        || pattern.contains("(?")
        || (1..=9).any(|digit| pattern.contains(&format!("\\{digit}")))
}
