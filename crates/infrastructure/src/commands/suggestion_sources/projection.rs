//! 三种远端来源共享的候选边界、评分和文本转义规则。
//!
//! @author kongweiguang

use std::hash::{Hash, Hasher};

use kerminal_domain::{
    commands::{
        CommandSuggestionActivation, CommandSuggestionCandidate, CommandSuggestionReplacementRange,
        CommandSuggestionRequest, CommandSuggestionSensitivity, SuggestionProviderKind,
    },
    files::RemotePath,
    identity::HostId,
};

use super::{RemoteCommandSuggestionProvider, syntax};

/// 遵从调用方显式 provider 选择；未提供筛选表示当前请求允许所有远端能力。
pub(super) fn requested(
    request: &CommandSuggestionRequest,
    provider: SuggestionProviderKind,
) -> bool {
    request
        .providers
        .as_ref()
        .is_none_or(|providers| providers.contains(&provider))
}

/// 统一绑定请求、来源与替换起点；候选展示数据不能改变其 session generation。
#[derive(Clone, Copy)]
pub(super) struct CandidateContext<'a> {
    pub(super) request: &'a CommandSuggestionRequest,
    pub(super) provider: SuggestionProviderKind,
    pub(super) token_start: usize,
}

/// 构造候选时将请求绑定和展示负载分开，确保 range 始终源自用户当前 token。
pub(super) fn candidate(
    context: CandidateContext<'_>,
    display_text: String,
    replacement_text: String,
    description: &'static str,
    score: f64,
    source_identity: &str,
) -> CommandSuggestionCandidate {
    let CandidateContext {
        request,
        provider,
        token_start,
    } = context;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    provider.hash(&mut hasher);
    request.remote_host_id.hash(&mut hasher);
    request.session_id.hash(&mut hasher);
    request.cwd.hash(&mut hasher);
    source_identity.hash(&mut hasher);
    replacement_text.hash(&mut hasher);
    let id = format!("remote:{:016x}", hasher.finish());
    CommandSuggestionCandidate {
        id: id.clone(),
        provider,
        activation: CommandSuggestionActivation::Insert,
        display_text,
        replacement_text,
        replacement_range: CommandSuggestionReplacementRange {
            start: token_start,
            end: request.cursor,
        },
        suffix: String::new(),
        score,
        sensitivity: CommandSuggestionSensitivity::Normal,
        description: Some(description.to_owned()),
        source_explanation: Some(match provider {
            SuggestionProviderKind::RemotePath => "来自当前 SSH 主机的一层目录项".to_owned(),
            SuggestionProviderKind::RemoteCommand => {
                "来自当前 SSH 主机的 PATH 和 shell 命令".to_owned()
            }
            SuggestionProviderKind::Git => "来自当前 SSH 主机的 Git 只读信息".to_owned(),
            _ => "来自远端建议来源".to_owned(),
        }),
        merged_source_explanations: Vec::new(),
        source_id: Some(id),
        allowed_presentations: CommandSuggestionCandidate::allowed_for(
            CommandSuggestionSensitivity::Normal,
        ),
        accept_boundaries: Vec::new(),
        context_key: request.context_key.clone(),
    }
}

/// 收敛 Git cache 键到主机与工作目录，避免相同路径的其它 SSH 主机复用 refs。
pub(super) fn command_cache_key_for_git(
    host_id: &HostId,
    request: &CommandSuggestionRequest,
) -> (String, String) {
    RemoteCommandSuggestionProvider::cwd_cache_key(host_id, request)
}

/// 按安全 shell 语法转义远端名称；未知语法只允许跨 shell 裸 token。
pub(super) fn quote(value: &str, shell: syntax::ShellFamily) -> Option<String> {
    syntax::quote_literal(value, shell)
}

/// 让精确短前缀靠前，并使目录略高于普通路径文件。
pub(super) fn prefix_score(prefix: &str, candidate: &str, directory: bool) -> f64 {
    let suffix = candidate
        .chars()
        .count()
        .saturating_sub(prefix.chars().count());
    let length_score = 0.9 - (suffix.min(500) as f64 / 10_000.0);
    length_score + if directory { 0.025 } else { 0.0 }
}

/// 让 SFTP 路径统一经过领域规范化器，而不是依据客户端操作系统分隔符解析。
pub(super) fn parse_remote_path(value: &str) -> Option<RemotePath> {
    RemotePath::parse(value).ok()
}
