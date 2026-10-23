//! 片段正文准备：直接变量替换与保留的受管 shell 转义入口。
//!
//! @author kongweiguang

use std::collections::{BTreeMap, HashMap};

use kerminal_domain::{
    commands::{
        SnippetCatalogItem, SnippetContextBindingKind, SnippetDefinition, SnippetScope,
        SnippetVariable, SnippetVariableKind, SnippetVariableRenderStrategy,
    },
    connections::RemoteShellDialect,
    identity::SessionId,
    sessions::{SessionInfo, SessionTarget},
};
use regex::Regex;

use super::{AppError, AppResult, CommandsService};

/// 渲染器只支持经过审查的 POSIX shell 与 PowerShell 单参数转义规则。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnippetRenderDialect {
    Posix,
    Fish,
    PowerShell,
}

impl SnippetRenderDialect {
    /// 只接受运行时已确认的 shell 方言，避免把 SSH 协议或未知 shell 猜成 POSIX。
    ///
    /// @author kongweiguang
    pub fn from_trusted_shell(dialect: RemoteShellDialect) -> AppResult<Self> {
        match dialect {
            RemoteShellDialect::Posix => Ok(Self::Posix),
            RemoteShellDialect::Fish => Ok(Self::Fish),
            RemoteShellDialect::PowerShell => Ok(Self::PowerShell),
            RemoteShellDialect::Unknown => Err(AppError::capability_not_supported(
                "当前 shell 未提供安全的命令片段转义规则",
            )),
        }
    }
}

/// 预览结果标注是否含用户输入的秘密，调用方据此禁止写入历史和遥测。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnippetRenderResult {
    pub command: String,
    pub sensitive: bool,
}

/// 直接发送按定义名替换的变量；未知或未提供的占位符保留原文，避免把不完整模板改写成别的命令。
///
/// 此路径刻意不推断目标平台、shell、scope 或变量类型，也不做 shell quoting；调用方负责把结果送入用户选定的终端。
///
/// @author kongweiguang
pub fn direct_snippet_command(
    definition: &SnippetDefinition,
    values: &BTreeMap<String, String>,
) -> SnippetRenderResult {
    let mut command = String::with_capacity(definition.command.len());
    let mut remaining = definition.command.as_str();
    let mut sensitive = false;

    while let Some(open) = remaining.find("{{") {
        command.push_str(&remaining[..open]);
        let after_open = &remaining[open + 2..];
        let Some(close) = after_open.find("}}") else {
            command.push_str(&remaining[open..]);
            remaining = "";
            break;
        };

        let placeholder_end = open + 2 + close + 2;
        let placeholder = &remaining[open..placeholder_end];
        let name = &after_open[..close];
        let variable = definition
            .variables
            .iter()
            .find(|variable| variable.name == name);
        let replacement = variable.and_then(|variable| {
            values
                .get(&variable.name)
                .or(variable.default_value.as_ref())
                .map(|value| {
                    sensitive |= variable.sensitive || variable.kind == SnippetVariableKind::Secret;
                    value.as_str()
                })
        });

        if let Some(replacement) = replacement {
            command.push_str(replacement);
        } else {
            command.push_str(placeholder);
        }
        remaining = &remaining[placeholder_end..];
    }
    command.push_str(remaining);

    SnippetRenderResult { command, sensitive }
}

/// 从目录行复用同一直接替换逻辑；平台和 shell 元数据仅供展示，不参与是否可发送的判断。
///
/// @author kongweiguang
pub fn direct_snippet_catalog_command(
    item: &SnippetCatalogItem,
    values: &BTreeMap<String, String>,
) -> SnippetRenderResult {
    let definition = super::snippet_catalog::definition_from_catalog_item(item);
    let mut rendered = direct_snippet_command(&definition, values);
    rendered.sensitive |= item.sensitive;
    rendered
}

impl CommandsService {
    /// 读取用户片段并在本地渲染，变量值只停留在内存中并由领域策略约束。
    pub async fn render_user_snippet(
        &self,
        snippet_id: String,
        values: BTreeMap<String, String>,
        dialect: SnippetRenderDialect,
    ) -> AppResult<SnippetRenderResult> {
        let definition = self
            .get_user_snippet(snippet_id)
            .await?
            .ok_or_else(|| AppError::not_found("用户命令片段不存在"))?;
        render_snippet_definition(&definition, &values, dialect)
    }

    /// 按明确会话校验片段作用域，并从 session runtime 读取已确认方言，不按 SSH 类型推断 shell。
    pub async fn render_snippet_for_session(
        &self,
        snippet_id: String,
        session_id: SessionId,
        values: BTreeMap<String, String>,
    ) -> AppResult<SnippetRenderResult> {
        let definition = self
            .get_user_snippet(snippet_id)
            .await?
            .ok_or_else(|| AppError::not_found("用户命令片段不存在"))?;
        let session = self.session_info(session_id).await?;
        validate_snippet_session(&definition, &session)?;
        let dialect = self.trusted_render_dialect(&session).await?;
        render_snippet_definition(&definition, &values, dialect)
    }

    /// 渲染只读 catalog 的内置定义；由应用层复用同一作用域校验，避免 UI 绕过领域边界。
    pub async fn render_definition_for_session(
        &self,
        definition: SnippetDefinition,
        session_id: SessionId,
        values: BTreeMap<String, String>,
    ) -> AppResult<SnippetRenderResult> {
        let session = self.session_info(session_id).await?;
        validate_snippet_session(&definition, &session)?;
        let dialect = self.trusted_render_dialect(&session).await?;
        render_snippet_definition(&definition, &values, dialect)
    }

    /// 片段填入或执行前复核真实 runtime owner 与运行代次，阻止旧 session id 转绑到重连实例。
    ///
    /// @author kongweiguang
    pub async fn render_snippet_for_binding(
        &self,
        definition: &SnippetDefinition,
        binding: &crate::sessions::UiSessionBinding,
        values: &BTreeMap<String, String>,
    ) -> AppResult<SnippetRenderResult> {
        let session = self.session_info(binding.session_id.clone()).await?;
        if session.lifecycle != kerminal_domain::sessions::SessionLifecycle::Running
            || session.owner != binding.owner
        {
            return Err(AppError::conflict("片段目标会话已变化或已关闭"));
        }
        validate_snippet_session(definition, &session)?;
        let context = self
            .sessions
            .terminal_command_context(binding.session_id.clone())
            .await?;
        if context.session_id != binding.session_id
            || context.owner != binding.owner
            || context.session_generation != binding.session_generation
        {
            return Err(AppError::conflict("片段目标会话代次已变化"));
        }
        let dialect = SnippetRenderDialect::from_trusted_shell(context.shell_dialect)?;
        render_snippet_definition(definition, values, dialect)
    }

    /// 只从 runtime context 取方言；metadata 缺失或未知时拒绝生成可能错误引用的命令。
    ///
    /// @author kongweiguang
    async fn trusted_render_dialect(
        &self,
        session: &SessionInfo,
    ) -> AppResult<SnippetRenderDialect> {
        let context = self
            .sessions
            .terminal_command_context(session.session_id.clone())
            .await?;
        if context.session_id != session.session_id || context.owner != session.owner {
            return Err(AppError::conflict("片段目标会话状态已变化"));
        }
        SnippetRenderDialect::from_trusted_shell(context.shell_dialect)
    }
}

/// 检查 Local/SSH 作用域，并拒绝无法从当前会话证明的 workspace 或 host-group 绑定。
fn validate_snippet_session(
    definition: &SnippetDefinition,
    session: &SessionInfo,
) -> AppResult<()> {
    let host_id = match &session.target {
        SessionTarget::Local(_) => {
            if definition.scope == SnippetScope::Ssh {
                return Err(AppError::conflict("命令片段不适用于本地终端"));
            }
            None
        }
        SessionTarget::Ssh(target) => {
            if definition.scope == SnippetScope::Local {
                return Err(AppError::conflict("命令片段不适用于 SSH 终端"));
            }
            Some(target.host_id.as_str())
        }
        SessionTarget::Telnet(_) | SessionTarget::Serial(_) => {
            if definition.scope != SnippetScope::Any {
                return Err(AppError::conflict("命令片段不适用于当前终端协议"));
            }
            None
        }
        SessionTarget::Container(target) => {
            if definition.scope != SnippetScope::Any {
                return Err(AppError::conflict("命令片段不适用于当前终端协议"));
            }
            target.host_id.as_ref().map(|host_id| host_id.as_str())
        }
    };
    for binding in &definition.context_bindings {
        let matches = match binding.kind {
            SnippetContextBindingKind::Global => true,
            SnippetContextBindingKind::Host => {
                host_id.is_some_and(|id| binding.target_id.as_deref() == Some(id))
            }
            SnippetContextBindingKind::Workspace | SnippetContextBindingKind::HostGroup => false,
        };
        if !matches {
            return Err(AppError::conflict("命令片段上下文绑定与当前会话不匹配"));
        }
    }
    Ok(())
}

/// 将双花括号模板替换为类型校验后的值，任何缺失或额外变量都在执行前失败。
pub fn render_snippet_definition(
    definition: &SnippetDefinition,
    values: &BTreeMap<String, String>,
    dialect: SnippetRenderDialect,
) -> AppResult<SnippetRenderResult> {
    definition
        .validate()
        .map_err(|_| AppError::invalid("命令片段定义未通过校验"))?;
    let known_names = definition
        .variables
        .iter()
        .map(|variable| variable.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    if values
        .keys()
        .any(|name| !known_names.contains(name.as_str()))
    {
        return Err(AppError::invalid("命令片段包含未声明变量输入"));
    }

    let mut resolved = HashMap::with_capacity(definition.variables.len());
    let mut sensitive = false;
    for variable in &definition.variables {
        let value = values
            .get(&variable.name)
            .or(variable.default_value.as_ref())
            .cloned()
            .unwrap_or_default();
        if variable.required && value.is_empty() {
            return Err(AppError::invalid("命令片段缺少必填变量"));
        }
        if value.is_empty() && !variable.required {
            let rendered = match variable.render_strategy {
                SnippetVariableRenderStrategy::ValidatedRaw => String::new(),
                SnippetVariableRenderStrategy::ShellArg
                | SnippetVariableRenderStrategy::Literal => quote_shell_argument(&value, dialect),
            };
            sensitive |= variable.sensitive || variable.kind == SnippetVariableKind::Secret;
            resolved.insert(variable.name.as_str(), rendered);
            continue;
        }
        validate_variable_value(variable, &value)?;
        let rendered = match variable.render_strategy {
            SnippetVariableRenderStrategy::ValidatedRaw => value,
            SnippetVariableRenderStrategy::ShellArg | SnippetVariableRenderStrategy::Literal => {
                quote_shell_argument(&value, dialect)
            }
        };
        sensitive |= variable.sensitive || variable.kind == SnippetVariableKind::Secret;
        resolved.insert(variable.name.as_str(), rendered);
    }

    let placeholders = Regex::new(r"\{\{([A-Za-z_][A-Za-z0-9_]*)\}\}")
        .map_err(|_| AppError::infrastructure("片段占位符解析器不可用"))?;
    let mut rendered = String::with_capacity(definition.command.len());
    let mut last_end = 0;
    for captures in placeholders.captures_iter(&definition.command) {
        let Some(matched) = captures.get(0) else {
            return Err(AppError::invalid("命令片段占位符不完整"));
        };
        let Some(name) = captures.get(1).map(|value| value.as_str()) else {
            return Err(AppError::invalid("命令片段占位符不完整"));
        };
        rendered.push_str(&definition.command[last_end..matched.start()]);
        let replacement = resolved
            .get(name)
            .ok_or_else(|| AppError::invalid("命令片段引用了未知变量"))?;
        rendered.push_str(replacement);
        last_end = matched.end();
    }
    rendered.push_str(&definition.command[last_end..]);
    if rendered.chars().count() > 16_000 {
        return Err(AppError::invalid("渲染后的命令超过长度限制"));
    }
    Ok(SnippetRenderResult {
        command: rendered,
        sensitive,
    })
}

/// 对变量类型和可选正则执行双重校验，避免模板把不可信文本解释为 shell 语法。
fn validate_variable_value(variable: &SnippetVariable, value: &str) -> AppResult<()> {
    if value.chars().count() > 4_000 || value.chars().any(char::is_control) {
        return Err(AppError::invalid("命令片段变量包含过长或控制字符"));
    }
    let type_valid = match variable.kind {
        SnippetVariableKind::Text | SnippetVariableKind::Path | SnippetVariableKind::Secret => true,
        SnippetVariableKind::Port => value.parse::<u16>().is_ok_and(|port| port > 0),
        SnippetVariableKind::Integer => value.parse::<i64>().is_ok(),
        SnippetVariableKind::Host => {
            !value.is_empty()
                && value
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | ':'))
        }
        SnippetVariableKind::Url => value.split_once("://").is_some_and(|(scheme, rest)| {
            !scheme.is_empty()
                && scheme
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '.' | '-'))
                && !rest.is_empty()
                && !rest.chars().any(char::is_whitespace)
        }),
        SnippetVariableKind::Service | SnippetVariableKind::Container => {
            !value.is_empty()
                && value
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
        }
        SnippetVariableKind::Enum => variable.suggestions.iter().any(|item| item == value),
        SnippetVariableKind::Raw => value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(ch, '_' | '.' | '/' | ':' | '@' | '+' | ',' | '=' | '-')
        }),
    };
    if !type_valid {
        return Err(AppError::invalid("命令片段变量不符合类型约束"));
    }
    if let Some(pattern) = variable.validation.as_deref() {
        let expression = Regex::new(&format!("^(?:{pattern})$"))
            .map_err(|_| AppError::invalid("命令片段变量校验规则无效"))?;
        if !expression.is_match(value) {
            return Err(AppError::invalid("命令片段变量未通过校验"));
        }
    }
    if variable.render_strategy == SnippetVariableRenderStrategy::ValidatedRaw
        && !value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(ch, '_' | '.' | '/' | ':' | '@' | '+' | ',' | '=' | '-')
        })
    {
        return Err(AppError::invalid("原样变量包含不允许的 shell 字符"));
    }
    Ok(())
}

/// 用对应 shell 的安全单引号构造参数，避免变量中的空格、引号和替换语法被执行。
fn quote_shell_argument(value: &str, dialect: SnippetRenderDialect) -> String {
    match dialect {
        SnippetRenderDialect::Posix => format!("'{}'", value.replace('\'', "'\\''")),
        SnippetRenderDialect::Fish => {
            let escaped = value.replace('\\', "\\\\").replace('\'', "\\'");
            format!("'{escaped}'")
        }
        SnippetRenderDialect::PowerShell => format!("'{}'", value.replace('\'', "''")),
    }
}
