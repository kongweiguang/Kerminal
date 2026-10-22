//! SSH 路径补全只枚举当前输入指向的单层目录。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_application::AppResult;
use kerminal_domain::{
    commands::{CommandSuggestionCandidate, SuggestionProviderKind},
    files::{FileEntryKind, RemotePath},
};

use super::{
    CachedDirectoryEntry, CandidateContext, RemoteCommandSuggestionProvider, RemoteSuggestionQuery,
    candidate, prefix_score, quote,
    syntax::{accepts_bare_path, command_name},
};

/// 仅对旧版支持的路径命令或显式路径片段查询当前目录的一层项。
pub(super) async fn suggestions(
    provider: &RemoteCommandSuggestionProvider,
    query: &RemoteSuggestionQuery<'_>,
) -> AppResult<Vec<CommandSuggestionCandidate>> {
    let request = query.request;
    let prefix = query.prefix;
    let host_id = query.host_id;
    let command = command_name(prefix);
    let path_context = prefix.token.value.contains('/')
        || prefix.token.value.contains('\\')
        || command.is_some_and(accepts_bare_path);
    if !path_context {
        return Ok(Vec::new());
    }

    let (typed_directory, basename_prefix) = split_path_prefix(&prefix.token.value);
    let Some(directory) = resolve_directory(typed_directory, request.cwd.as_deref()) else {
        return Ok(Vec::new());
    };
    let entries = provider
        .directory_entries(
            SuggestionProviderKind::RemotePath,
            host_id,
            &directory,
            query.cancellation,
            query.operation_deadline,
        )
        .await?;
    let Some(entries) = entries else {
        return Ok(Vec::new());
    };
    Ok(path_candidates(
        query,
        &directory,
        &entries,
        typed_directory,
        basename_prefix,
    ))
}

/// 把当前路径 token 分成用户可见的目录前缀与待补全 basename。
fn split_path_prefix(value: &str) -> (&str, &str) {
    match value.rfind('/') {
        Some(separator) => (&value[..=separator], &value[separator + 1..]),
        None => ("", value),
    }
}

/// 相对用户输入路径按 session cwd 查询，绝对路径直接使用，RemotePath 负责规范分隔符。
fn resolve_directory(typed_directory: &str, cwd: Option<&str>) -> Option<RemotePath> {
    let value = if typed_directory.starts_with('/') {
        typed_directory.to_owned()
    } else {
        let base = cwd.filter(|value| !value.trim().is_empty()).unwrap_or(".");
        let base = RemotePath::parse(base).ok()?;
        if typed_directory.is_empty() {
            base.as_str().to_owned()
        } else if base.as_str() == "." {
            typed_directory.trim_end_matches('/').to_owned()
        } else {
            format!("{}/{typed_directory}", base.as_str().trim_end_matches('/'))
        }
    };
    RemotePath::parse(&value).ok()
}

/// 候选名称只来自当前远端目录；未知 shell 丢弃无法安全裸插入的特殊文件名。
fn path_candidates(
    query: &RemoteSuggestionQuery<'_>,
    directory: &RemotePath,
    entries: &Arc<Vec<CachedDirectoryEntry>>,
    typed_directory: &str,
    basename_prefix: &str,
) -> Vec<CommandSuggestionCandidate> {
    let request = query.request;
    let prefix = query.prefix;
    let host_id = query.host_id;
    let shell = prefix.shell;
    let mut candidates = entries
        .iter()
        .filter_map(|entry| {
            if !entry.name.starts_with(basename_prefix) {
                return None;
            }
            let is_directory = entry.kind == FileEntryKind::Directory;
            let mut completed = format!("{typed_directory}{}", entry.name);
            if is_directory {
                completed.push('/');
            }
            let replacement = quote(&completed, shell)?;
            let display = if is_directory {
                format!("{}/", entry.name)
            } else {
                entry.name.clone()
            };
            let description = if is_directory {
                "远端目录"
            } else {
                "远端文件"
            };
            let score = prefix_score(basename_prefix, &entry.name, is_directory);
            let identity = format!("{}:{}", directory.as_str(), entry.name);
            Some(candidate(
                CandidateContext {
                    request,
                    provider: SuggestionProviderKind::RemotePath,
                    token_start: prefix.token.start,
                },
                display,
                replacement,
                description,
                score,
                &format!("{}:{identity}", host_id.as_str()),
            ))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.display_text.cmp(&right.display_text))
    });
    candidates.truncate(super::MAX_PATH_ENTRIES);
    candidates
}
