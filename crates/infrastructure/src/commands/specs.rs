//! 基于冻结的 command-specs/v1 资源提供本地静态补全。
//!
//! @author kongweiguang

#[path = "specs/schema.rs"]
mod schema;

use async_trait::async_trait;
use kerminal_application::{AppError, AppResult, commands::CommandSuggestionProvider};
use kerminal_domain::commands::{
    CommandHistoryTarget, CommandSuggestionActivation, CommandSuggestionCandidate,
    CommandSuggestionReplacementRange, CommandSuggestionRequest, CommandSuggestionSensitivity,
    SuggestionPresentation, SuggestionProviderKind, SuggestionQueryMode,
};
use tokio_util::sync::CancellationToken;

use self::schema::{ArgumentKind, CommandSpec, SafetySpec, Sensitivity, Shell};

const SPEC_SOURCES: &[(&str, &str)] = &[
    (
        "containers/docker.toml",
        include_str!("../../resources/command-specs/v1/containers/docker.toml"),
    ),
    (
        "containers/podman.toml",
        include_str!("../../resources/command-specs/v1/containers/podman.toml"),
    ),
    (
        "development/cargo.toml",
        include_str!("../../resources/command-specs/v1/development/cargo.toml"),
    ),
    (
        "development/go.toml",
        include_str!("../../resources/command-specs/v1/development/go.toml"),
    ),
    (
        "development/gradle.toml",
        include_str!("../../resources/command-specs/v1/development/gradle.toml"),
    ),
    (
        "development/java.toml",
        include_str!("../../resources/command-specs/v1/development/java.toml"),
    ),
    (
        "development/mvn.toml",
        include_str!("../../resources/command-specs/v1/development/mvn.toml"),
    ),
    (
        "development/node.toml",
        include_str!("../../resources/command-specs/v1/development/node.toml"),
    ),
    (
        "development/npm.toml",
        include_str!("../../resources/command-specs/v1/development/npm.toml"),
    ),
    (
        "development/pip.toml",
        include_str!("../../resources/command-specs/v1/development/pip.toml"),
    ),
    (
        "development/pnpm.toml",
        include_str!("../../resources/command-specs/v1/development/pnpm.toml"),
    ),
    (
        "development/python.toml",
        include_str!("../../resources/command-specs/v1/development/python.toml"),
    ),
    (
        "development/rustup.toml",
        include_str!("../../resources/command-specs/v1/development/rustup.toml"),
    ),
    (
        "development/yarn.toml",
        include_str!("../../resources/command-specs/v1/development/yarn.toml"),
    ),
    (
        "infrastructure/ansible.toml",
        include_str!("../../resources/command-specs/v1/infrastructure/ansible.toml"),
    ),
    (
        "infrastructure/aws.toml",
        include_str!("../../resources/command-specs/v1/infrastructure/aws.toml"),
    ),
    (
        "infrastructure/az.toml",
        include_str!("../../resources/command-specs/v1/infrastructure/az.toml"),
    ),
    (
        "infrastructure/gcloud.toml",
        include_str!("../../resources/command-specs/v1/infrastructure/gcloud.toml"),
    ),
    (
        "infrastructure/terraform.toml",
        include_str!("../../resources/command-specs/v1/infrastructure/terraform.toml"),
    ),
    (
        "orchestration/helm.toml",
        include_str!("../../resources/command-specs/v1/orchestration/helm.toml"),
    ),
    (
        "orchestration/kubectl.toml",
        include_str!("../../resources/command-specs/v1/orchestration/kubectl.toml"),
    ),
    (
        "process/journalctl.toml",
        include_str!("../../resources/command-specs/v1/process/journalctl.toml"),
    ),
    (
        "process/kill.toml",
        include_str!("../../resources/command-specs/v1/process/kill.toml"),
    ),
    (
        "process/ps.toml",
        include_str!("../../resources/command-specs/v1/process/ps.toml"),
    ),
    (
        "process/top.toml",
        include_str!("../../resources/command-specs/v1/process/top.toml"),
    ),
    (
        "remote/dig.toml",
        include_str!("../../resources/command-specs/v1/remote/dig.toml"),
    ),
    (
        "remote/ip.toml",
        include_str!("../../resources/command-specs/v1/remote/ip.toml"),
    ),
    (
        "remote/netstat.toml",
        include_str!("../../resources/command-specs/v1/remote/netstat.toml"),
    ),
    (
        "remote/ping.toml",
        include_str!("../../resources/command-specs/v1/remote/ping.toml"),
    ),
    (
        "remote/rsync.toml",
        include_str!("../../resources/command-specs/v1/remote/rsync.toml"),
    ),
    (
        "remote/scp.toml",
        include_str!("../../resources/command-specs/v1/remote/scp.toml"),
    ),
    (
        "remote/ss.toml",
        include_str!("../../resources/command-specs/v1/remote/ss.toml"),
    ),
    (
        "remote/ssh.toml",
        include_str!("../../resources/command-specs/v1/remote/ssh.toml"),
    ),
    (
        "system/awk.toml",
        include_str!("../../resources/command-specs/v1/system/awk.toml"),
    ),
    (
        "system/cd.toml",
        include_str!("../../resources/command-specs/v1/system/cd.toml"),
    ),
    (
        "system/cp.toml",
        include_str!("../../resources/command-specs/v1/system/cp.toml"),
    ),
    (
        "system/curl.toml",
        include_str!("../../resources/command-specs/v1/system/curl.toml"),
    ),
    (
        "system/find.toml",
        include_str!("../../resources/command-specs/v1/system/find.toml"),
    ),
    (
        "system/grep.toml",
        include_str!("../../resources/command-specs/v1/system/grep.toml"),
    ),
    (
        "system/ls.toml",
        include_str!("../../resources/command-specs/v1/system/ls.toml"),
    ),
    (
        "system/mkdir.toml",
        include_str!("../../resources/command-specs/v1/system/mkdir.toml"),
    ),
    (
        "system/mv.toml",
        include_str!("../../resources/command-specs/v1/system/mv.toml"),
    ),
    (
        "system/pwd.toml",
        include_str!("../../resources/command-specs/v1/system/pwd.toml"),
    ),
    (
        "system/rm.toml",
        include_str!("../../resources/command-specs/v1/system/rm.toml"),
    ),
    (
        "system/sed.toml",
        include_str!("../../resources/command-specs/v1/system/sed.toml"),
    ),
    (
        "system/systemctl.toml",
        include_str!("../../resources/command-specs/v1/system/systemctl.toml"),
    ),
    (
        "system/tar.toml",
        include_str!("../../resources/command-specs/v1/system/tar.toml"),
    ),
    (
        "system/wget.toml",
        include_str!("../../resources/command-specs/v1/system/wget.toml"),
    ),
    (
        "version-control/git.toml",
        include_str!("../../resources/command-specs/v1/version-control/git.toml"),
    ),
];

const MAX_SPEC_CANDIDATES: usize = 100;
const MAX_PREFIX_CHARS: usize = 4_000;
const MAX_PREFIX_BYTES: usize = 32_000;

/// 提供无网络、无 shell 执行副作用的内置命令建议。
pub struct StaticSpecSuggestionProvider {
    specs: Vec<CommandSpec>,
}

impl StaticSpecSuggestionProvider {
    /// 初始化时一次解析随程序发布的资源；运行时查询只遍历内存，不触碰用户文件。
    pub fn new() -> AppResult<Self> {
        let mut specs = Vec::with_capacity(SPEC_SOURCES.len());
        for (_, source) in SPEC_SOURCES {
            let spec = toml::from_str::<CommandSpec>(source).map_err(|error| {
                AppError::infrastructure_with_source("内置命令建议资源无效", error)
            })?;
            if spec.schema_version != 1 || spec.command.trim().is_empty() {
                return Err(AppError::infrastructure("内置命令建议资源版本或命令名无效"));
            }
            specs.push(spec);
        }
        specs.sort_by(|left, right| left.command.cmp(&right.command));
        Ok(Self { specs })
    }

    /// 返回随程序一同编译的 spec 数量，供接入验收确认资源没有漏装。
    pub fn command_count(&self) -> usize {
        self.specs.len()
    }
}

#[async_trait]
impl CommandSuggestionProvider for StaticSpecSuggestionProvider {
    /// 该 adapter 只负责冻结的 spec 资源，不与 history、snippet 或远端来源竞争归属。
    fn supported_providers(&self) -> Vec<SuggestionProviderKind> {
        vec![SuggestionProviderKind::Spec]
    }

    /// 根据当前 cursor 前缀匹配命令、子命令、选项与唯一枚举参数，并尊重模式安全标记。
    async fn suggest(
        &self,
        request: CommandSuggestionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<CommandSuggestionCandidate>> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("命令建议查询已取消"));
        }
        if request
            .providers
            .as_ref()
            .is_some_and(|providers| !providers.contains(&SuggestionProviderKind::Spec))
        {
            return Ok(Vec::new());
        }
        let Some(shell) = request.shell.as_deref().and_then(parse_shell) else {
            return Ok(Vec::new());
        };
        let Some(prefix) = safe_prefix(&request) else {
            return Ok(Vec::new());
        };
        let Some(context) = PrefixContext::parse(prefix) else {
            return Ok(Vec::new());
        };

        let mut candidates = if context.completed.is_empty() {
            self.root_commands(&request, &context, shell)
        } else {
            let command = &context.completed[0];
            let Some(spec) = self.find_command(command, request.target, shell) else {
                return Ok(Vec::new());
            };
            self.command_items(&request, &context, spec)
        };
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("命令建议查询已取消"));
        }
        candidates.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.display_text.cmp(&right.display_text))
                .then_with(|| left.id.cmp(&right.id))
        });
        candidates.truncate(request.limit.unwrap_or(20).clamp(1, MAX_SPEC_CANDIDATES));
        Ok(candidates)
    }
}

impl StaticSpecSuggestionProvider {
    /// 根命令只在首个简单 token 上补全，别名只用于识别上下文而不假设已安装。
    fn root_commands(
        &self,
        request: &CommandSuggestionRequest,
        context: &PrefixContext,
        shell: Shell,
    ) -> Vec<CommandSuggestionCandidate> {
        self.specs
            .iter()
            .filter(|spec| supports_context(spec, request.target, shell))
            .filter(|spec| starts_with_folded(&spec.command, &context.current))
            .filter_map(|spec| {
                self.candidate(
                    request,
                    spec,
                    &context.current,
                    &spec.command,
                    &spec.description,
                    &spec.safety,
                    context.current_range.clone(),
                    context.current.chars().count(),
                    0.9,
                )
            })
            .collect()
    }

    /// 已识别命令后只补其直接子命令、适用选项或唯一 enum 参数。
    fn command_items(
        &self,
        request: &CommandSuggestionRequest,
        context: &PrefixContext,
        spec: &CommandSpec,
    ) -> Vec<CommandSuggestionCandidate> {
        let path = active_subcommand_path(spec, &context.completed[1..]);
        let Some(path_safety) = safety_for_path(spec, &path) else {
            return Vec::new();
        };
        let mut candidates = Vec::new();
        let current = &context.current;

        candidates.extend(self.option_argument_candidates(
            request,
            context,
            spec,
            &path,
            &path_safety,
        ));

        if current.starts_with('-') || current.is_empty() {
            for option in spec.options.iter().filter(|option| {
                (option.path.is_empty() || option.path == path)
                    && option_is_available(option, &context.completed)
                    && (option.name.starts_with(current)
                        || option
                            .aliases
                            .iter()
                            .any(|alias| alias.starts_with(current)))
            }) {
                let safety = combine_safety(&path_safety, &option.safety);
                let mut description = option.description.clone();
                if let Some(argument) = &option.argument
                    && !argument.placeholder.is_empty()
                {
                    description.push_str(" · ");
                    description.push_str(&argument.placeholder);
                }
                if let Some(candidate) = self.candidate(
                    request,
                    spec,
                    current,
                    &option.name,
                    &description,
                    &safety,
                    context.current_range.clone(),
                    current.chars().count(),
                    0.88,
                ) {
                    candidates.push(candidate);
                }
            }
        }

        if !current.starts_with('-') {
            let path_is_complete = context.completed[1..]
                .iter()
                .all(|token| is_subcommand_path_token(spec, token));
            if path_is_complete {
                for subcommand in spec.subcommands.iter().filter(|subcommand| {
                    subcommand.path == path
                        && (subcommand.name.starts_with(current)
                            || subcommand
                                .aliases
                                .iter()
                                .any(|alias| alias.starts_with(current)))
                }) {
                    let safety = combine_safety(&path_safety, &subcommand.safety);
                    if let Some(candidate) = self.candidate(
                        request,
                        spec,
                        current,
                        &subcommand.name,
                        &subcommand.description,
                        &safety,
                        context.current_range.clone(),
                        current.chars().count(),
                        0.92,
                    ) {
                        candidates.push(candidate);
                    }
                }
                candidates.extend(self.enum_argument_candidates(
                    request,
                    context,
                    spec,
                    &path,
                    &path_safety,
                ));
            }
        }
        candidates
    }

    /// 只建议唯一且明确标为 enum 的位置参数，避免猜測普通 path、host 或自由文本。
    fn enum_argument_candidates(
        &self,
        request: &CommandSuggestionRequest,
        context: &PrefixContext,
        spec: &CommandSpec,
        path: &[String],
        path_safety: &SafetySpec,
    ) -> Vec<CommandSuggestionCandidate> {
        let arguments = spec
            .arguments
            .iter()
            .filter(|argument| argument.path.is_empty() || argument.path == path)
            .collect::<Vec<_>>();
        let index = positional_argument_index(spec, &context.completed, path);
        let Some(argument) = arguments
            .get(index)
            .copied()
            .filter(|argument| argument.kind == ArgumentKind::Enum)
        else {
            return Vec::new();
        };
        if !relationships_allow(&argument.relationships, &context.completed) {
            return Vec::new();
        }
        let safety = combine_safety(path_safety, &argument.safety);
        let named_description = if argument.name.is_empty() {
            argument.description.clone()
        } else {
            format!("{} · {}", argument.name, argument.description)
        };
        let description = if argument.required {
            format!("{} · required", named_description)
        } else {
            named_description
        };
        argument
            .values
            .iter()
            .filter(|value| starts_with_folded(value, &context.current))
            .filter_map(|value| {
                self.candidate(
                    request,
                    spec,
                    &context.current,
                    value,
                    &description,
                    &safety,
                    context.current_range.clone(),
                    context.current.chars().count(),
                    0.82,
                )
            })
            .collect()
    }

    /// 只对 spec 明确列出的有限选项参数值做补全，不猜自由路径或远程资源。
    fn option_argument_candidates(
        &self,
        request: &CommandSuggestionRequest,
        context: &PrefixContext,
        spec: &CommandSpec,
        path: &[String],
        path_safety: &SafetySpec,
    ) -> Vec<CommandSuggestionCandidate> {
        let (typed_value, option_name, inline_value) =
            if let Some((name, value)) = context.current.split_once('=') {
                (value, name, true)
            } else if context.current.is_empty() {
                let Some(name) = context.completed.last().map(String::as_str) else {
                    return Vec::new();
                };
                ("", name, false)
            } else {
                return Vec::new();
            };
        let Some(option) = spec.options.iter().find(|option| {
            (option.path.is_empty() || option.path == path)
                && (option.name == option_name || option.aliases.iter().any(|v| v == option_name))
        }) else {
            return Vec::new();
        };
        let Some(argument) = option.argument.as_ref() else {
            return Vec::new();
        };
        if argument.values.is_empty() || !option_is_available(option, &context.completed) {
            return Vec::new();
        }
        let safety = combine_safety(path_safety, &option.safety);
        argument
            .values
            .iter()
            .filter(|value| starts_with_folded(value, typed_value))
            .filter_map(|value| {
                let replacement = if inline_value {
                    format!("{option_name}={value}")
                } else {
                    value.clone()
                };
                let display = if inline_value {
                    replacement.as_str()
                } else {
                    value.as_str()
                };
                self.candidate(
                    request,
                    spec,
                    &context.current,
                    display,
                    &option.description,
                    &safety,
                    context.current_range.clone(),
                    context.current.chars().count(),
                    0.84,
                )
                .map(|mut candidate| {
                    candidate.replacement_text = replacement;
                    candidate
                })
            })
            .collect()
    }

    /// 统一应用展示安全标记并保留 spec 来源和当前请求的 replacement range。
    #[allow(clippy::too_many_arguments)]
    fn candidate(
        &self,
        request: &CommandSuggestionRequest,
        spec: &CommandSpec,
        typed: &str,
        value: &str,
        description: &str,
        safety: &SafetySpec,
        range: std::ops::Range<usize>,
        typed_len: usize,
        base_score: f64,
    ) -> Option<CommandSuggestionCandidate> {
        let allowed_presentations = allowed_presentations(safety);
        let requested = match request.mode {
            SuggestionQueryMode::Inline => SuggestionPresentation::Inline,
            SuggestionQueryMode::Menu => SuggestionPresentation::Menu,
        };
        if !allowed_presentations.contains(&requested) {
            return None;
        }
        let source_explanation = (!spec.source.name.is_empty()).then(|| {
            format!(
                "{} · {} · {} · {} · {}",
                spec.source.name, spec.source.url, spec.tested_version, spec.owner, spec.updated_at
            )
        });
        let description = safety
            .warning
            .as_deref()
            .filter(|warning| !warning.is_empty())
            .unwrap_or(description);
        let score = base_score + (typed_len as f64 / value.chars().count().max(1) as f64) * 0.08;
        Some(CommandSuggestionCandidate {
            id: format!("spec:{}:{}", spec.command, value),
            provider: SuggestionProviderKind::Spec,
            activation: CommandSuggestionActivation::Insert,
            display_text: value.to_owned(),
            replacement_text: value.to_owned(),
            replacement_range: CommandSuggestionReplacementRange {
                start: range.start,
                end: range.end,
            },
            suffix: value.chars().skip(typed.chars().count()).collect(),
            score,
            sensitivity: command_sensitivity(safety.sensitivity),
            description: Some(description.to_owned()),
            source_explanation,
            merged_source_explanations: Vec::new(),
            source_id: Some(spec.command.clone()),
            allowed_presentations,
            accept_boundaries: Vec::new(),
            context_key: request.context_key.clone(),
        })
    }

    /// 以规范名或冻结资源声明的别名定位命令，并按本机平台和已知 shell 收窄。
    fn find_command(
        &self,
        command: &str,
        target: CommandHistoryTarget,
        shell: Shell,
    ) -> Option<&CommandSpec> {
        self.specs.iter().find(|spec| {
            (spec.command.eq_ignore_ascii_case(command)
                || spec
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(command)))
                && supports_context(spec, target, shell)
        })
    }
}

/// 将可能含有未完成 shell 语法的行排除，避免静态补全改写引号或控制运算符。
fn safe_prefix(request: &CommandSuggestionRequest) -> Option<&str> {
    if request.cursor != request.input.chars().count() {
        return None;
    }
    let prefix = request.input.as_str();
    if prefix.chars().count() > MAX_PREFIX_CHARS
        || prefix.len() > MAX_PREFIX_BYTES
        || prefix.chars().any(char::is_control)
        || prefix.chars().any(|character| {
            matches!(
                character,
                '\'' | '"' | '`' | '\\' | '|' | '&' | ';' | '<' | '>' | '$'
            )
        })
    {
        return None;
    }
    Some(prefix)
}

/// 根据已声明的选项参数跳过 option value，只在当前枚举位置正好待输入时给出候选。
fn positional_argument_index(
    spec: &CommandSpec,
    completed: &[String],
    expected_path: &[String],
) -> usize {
    let mut path = Vec::<String>::new();
    let mut awaiting_option_value = false;
    let mut positional_index = 0;
    for token in completed.iter().skip(1) {
        if awaiting_option_value {
            awaiting_option_value = false;
            continue;
        }
        if token.starts_with('-') {
            let (name, inline_value) = token
                .split_once('=')
                .map_or((token.as_str(), false), |(name, _)| (name, true));
            let has_argument = spec.options.iter().any(|option| {
                (option.path.is_empty() || option.path == path)
                    && (option.name == name || option.aliases.iter().any(|alias| alias == name))
                    && option.argument.is_some()
            });
            awaiting_option_value = has_argument && !inline_value;
            continue;
        }
        if let Some(expected) = expected_path.get(path.len()) {
            let is_next_subcommand = spec.subcommands.iter().any(|subcommand| {
                subcommand.path == path
                    && subcommand.name == expected.as_str()
                    && (subcommand.name.eq_ignore_ascii_case(token)
                        || subcommand
                            .aliases
                            .iter()
                            .any(|alias| alias.eq_ignore_ascii_case(token)))
            });
            if is_next_subcommand {
                path.push(expected.clone());
                continue;
            }
        }
        positional_index += 1;
    }
    positional_index
}

/// 把无引号的前缀拆成已完成 token 与当前 token，范围使用 Unicode 标量索引。
fn parse_prefix(prefix: &str) -> PrefixContext {
    let mut completed = Vec::new();
    let mut current = String::new();
    let mut current_start = 0;
    let mut offset = 0;
    for token in prefix.split_inclusive(char::is_whitespace) {
        if token.chars().last().is_some_and(char::is_whitespace) {
            let content = token.trim_end_matches(char::is_whitespace);
            if !content.is_empty() {
                completed.push(content.to_owned());
            }
            offset += token.chars().count();
            current_start = offset;
        } else {
            current = token.to_owned();
            break;
        }
    }
    if prefix.is_empty() {
        current_start = 0;
    } else if prefix.chars().last().is_some_and(char::is_whitespace) {
        current.clear();
        current_start = prefix.chars().count();
    }
    let current_end = current_start + current.chars().count();
    PrefixContext {
        completed,
        current,
        current_range: current_start..current_end,
    }
}

/// 当前命令前缀切片；解析不了的 shell 控制语法会在此之前被拒绝。
struct PrefixContext {
    completed: Vec<String>,
    current: String,
    current_range: std::ops::Range<usize>,
}

impl PrefixContext {
    /// 构造当前 token 上下文，并拒绝空查询以免展示整套静态命令目录。
    fn parse(prefix: &str) -> Option<Self> {
        let context = parse_prefix(prefix);
        (!context.current.is_empty() || !context.completed.is_empty()).then_some(context)
    }
}

/// 解析请求中的 shell basename；未知方言不产生可能不兼容的补全。
fn parse_shell(value: &str) -> Option<Shell> {
    let basename = value
        .trim()
        .rsplit(['/', '\\'])
        .next()?
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    match basename.as_str() {
        "bash" => Some(Shell::Bash),
        "zsh" => Some(Shell::Zsh),
        "fish" => Some(Shell::Fish),
        "pwsh" | "powershell" | "powershell.exe" | "power_shell" => Some(Shell::Pwsh),
        "cmd" | "cmd.exe" => Some(Shell::Cmd),
        _ => None,
    }
}

/// 本地 session 使用当前进程平台，远端系统平台未知时只筛 shell 而不猜操作系统。
fn supports_context(spec: &CommandSpec, target: CommandHistoryTarget, shell: Shell) -> bool {
    spec.shells.contains(&shell)
        && (target != CommandHistoryTarget::Local
            || spec.platforms.iter().any(|platform| platform.is_current()))
}

/// 推进已知子命令路径；遇到未知参数后不再声称后续 token 是路径段。
fn active_subcommand_path(spec: &CommandSpec, completed: &[String]) -> Vec<String> {
    let mut path = Vec::new();
    for token in completed {
        if token.starts_with('-') {
            continue;
        }
        let Some(item) = spec.subcommands.iter().find(|item| {
            item.path == path
                && (item.name.eq_ignore_ascii_case(token)
                    || item
                        .aliases
                        .iter()
                        .any(|alias| alias.eq_ignore_ascii_case(token)))
        }) else {
            break;
        };
        path.push(item.name.clone());
    }
    path
}

/// 将每层已确认子命令的安全标记继承到后续选项和参数，危险操作始终只能主动展开菜单。
fn safety_for_path(spec: &CommandSpec, path: &[String]) -> Option<SafetySpec> {
    let mut safety = spec.safety.clone();
    let mut parent_path = Vec::<String>::new();
    for name in path {
        let subcommand = spec.subcommands.iter().find(|subcommand| {
            subcommand.path == parent_path && subcommand.name.eq_ignore_ascii_case(name)
        })?;
        safety = combine_safety(&safety, &subcommand.safety);
        parent_path.push(subcommand.name.clone());
    }
    Some(safety)
}

/// 检查命令选项的相互排斥和先决条件，避免显示明显无效的组合。
fn option_is_available(option: &schema::OptionSpec, completed: &[String]) -> bool {
    relationships_allow(&option.relationships, completed)
}

/// 按已输入的简单 token 执行静态 requires/conflicts 关系，不运行 shell 解释器。
fn relationships_allow(relationships: &schema::RelationshipSpec, completed: &[String]) -> bool {
    let seen = |name: &str| completed.iter().any(|token| token == name);
    !relationships.conflicts_with.iter().any(|name| seen(name))
        && relationships.requires.iter().all(|name| seen(name))
}

/// 防止在已识别参数后继续把未知普通 token 当成子命令路径。
fn is_subcommand_path_token(spec: &CommandSpec, token: &str) -> bool {
    token.starts_with('-')
        || spec.subcommands.iter().any(|item| {
            item.name.eq_ignore_ascii_case(token)
                || item
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(token))
        })
}

/// 对安全等级取最严格值，并使父级禁用 inline 的约束沿子项继承。
fn combine_safety(parent: &SafetySpec, item: &SafetySpec) -> SafetySpec {
    SafetySpec {
        sensitivity: parent.sensitivity.max(item.sensitivity),
        allow_inline: parent.allow_inline && item.allow_inline,
        warning: item.warning.clone().or_else(|| parent.warning.clone()),
    }
}

/// 把 spec 安全枚举映射到 UI 能识别的候选等级。
fn command_sensitivity(sensitivity: Sensitivity) -> CommandSuggestionSensitivity {
    match sensitivity {
        Sensitivity::Normal => CommandSuggestionSensitivity::Normal,
        Sensitivity::Dangerous => CommandSuggestionSensitivity::Dangerous,
        Sensitivity::Sensitive => CommandSuggestionSensitivity::Sensitive,
    }
}

/// 敏感候选始终隐藏，危险项仅在用户主动请求菜单时可见。
fn allowed_presentations(safety: &SafetySpec) -> Vec<SuggestionPresentation> {
    match safety.sensitivity {
        Sensitivity::Sensitive => Vec::new(),
        Sensitivity::Dangerous => vec![SuggestionPresentation::Menu],
        Sensitivity::Normal if safety.allow_inline => {
            vec![SuggestionPresentation::Inline, SuggestionPresentation::Menu]
        }
        Sensitivity::Normal => vec![SuggestionPresentation::Menu],
    }
}

/// 以大小写不敏感的前缀比较匹配跨平台命令名和 enum 值。
fn starts_with_folded(value: &str, prefix: &str) -> bool {
    value
        .chars()
        .take(prefix.chars().count())
        .collect::<String>()
        .eq_ignore_ascii_case(prefix)
}
