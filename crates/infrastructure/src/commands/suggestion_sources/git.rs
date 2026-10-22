//! Git 建议仅查询本地 refs 和 remote 名称，状态参数来自固定候选表。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_application::AppResult;
use kerminal_domain::{
    commands::{CommandSuggestionCandidate, CommandSuggestionRequest, SuggestionProviderKind},
    identity::HostId,
};

use super::{
    MAX_GIT_REFS, RemoteCommandSuggestionProvider, candidate, command_cache_key_for_git,
    prefix_score, quote,
    syntax::{ParsedPrefix, ShellFamily},
};

const GIT_SUBCOMMANDS: &[&str] = &[
    "add",
    "branch",
    "checkout",
    "cherry-pick",
    "clone",
    "commit",
    "diff",
    "fetch",
    "init",
    "log",
    "merge",
    "pull",
    "push",
    "rebase",
    "remote",
    "reset",
    "restore",
    "revert",
    "show",
    "stash",
    "status",
    "switch",
    "tag",
    "worktree",
];

const STATUS_OPTIONS: &[&str] = &[
    "--branch",
    "--ignored",
    "--porcelain",
    "--porcelain=v1",
    "--short",
    "--untracked-files=all",
    "-sb",
];

const REF_COMMANDS: &[&str] = &[
    "branch",
    "checkout",
    "cherry-pick",
    "diff",
    "log",
    "merge",
    "rebase",
    "revert",
    "show",
    "switch",
];

/// 仅分支引用和 remote 名称需要实时 Git 查询；静态子命令与 status 参数不建立连接。
pub(super) fn requires_remote_query(prefix: &ParsedPrefix) -> bool {
    let words = prefix
        .preceding_words
        .iter()
        .map(|word| word.value.as_str())
        .collect::<Vec<_>>();
    let typed = prefix.token.value.as_str();
    words.first().copied() == Some("git")
        && ((words.len() == 2 && REF_COMMANDS.contains(&words[1]) && !typed.starts_with('-'))
            || (words.len() == 2
                && matches!(words[1], "fetch" | "pull" | "push")
                && !typed.starts_with('-'))
            || (words.len() == 3
                && matches!(words[1], "fetch" | "pull" | "push")
                && !typed.starts_with('-')))
}

/// Git 前缀只在命令确为 git 时触发固定子命令、status 参数或只读 refs 查询。
pub(super) async fn suggestions(
    provider: &RemoteCommandSuggestionProvider,
    query: &super::RemoteSuggestionQuery<'_>,
) -> AppResult<Vec<CommandSuggestionCandidate>> {
    let request = query.request;
    let prefix = query.prefix;
    let host_id = query.host_id;
    let lease = query.lease;
    let words = prefix
        .preceding_words
        .iter()
        .map(|word| word.value.as_str())
        .collect::<Vec<_>>();
    if words.first().copied() != Some("git") {
        return Ok(Vec::new());
    }

    let typed = &prefix.token.value;
    let mut candidates = Vec::new();
    if words.len() == 1 {
        candidates.extend(static_candidates(
            query,
            typed,
            GIT_SUBCOMMANDS,
            "Git 子命令",
            "subcommand",
        ));
    } else if words.len() == 2
        && words[1] == "status"
        && (typed.is_empty() || typed.starts_with('-'))
    {
        candidates.extend(static_candidates(
            query,
            typed,
            STATUS_OPTIONS,
            "Git status 参数",
            "status-option",
        ));
    } else if words.len() == 2 && REF_COMMANDS.contains(&words[1]) && !typed.starts_with('-') {
        let refs = match lease {
            Some(_) => git_refs(provider, query).await?,
            None => None,
        };
        if let Some(refs) = refs {
            candidates.extend(ref_candidates(
                request,
                prefix,
                host_id,
                &refs,
                typed,
                prefix.shell,
            ));
        }
    } else if words.len() == 2
        && matches!(words[1], "fetch" | "pull" | "push")
        && !typed.starts_with('-')
    {
        let remotes = match lease {
            Some(_) => git_remotes(provider, query).await?,
            None => None,
        };
        if let Some(remotes) = remotes {
            let remote_names = remotes.iter().map(String::as_str).collect::<Vec<_>>();
            candidates.extend(static_candidates(
                query,
                typed,
                &remote_names,
                "Git remote",
                "remote-name",
            ));
        }
    } else if words.len() == 3
        && matches!(words[1], "fetch" | "pull" | "push")
        && !typed.starts_with('-')
    {
        let refs = match lease {
            Some(_) => git_refs(provider, query).await?,
            None => None,
        };
        if let Some(refs) = refs {
            candidates.extend(ref_candidates(
                request,
                prefix,
                host_id,
                &refs,
                typed,
                prefix.shell,
            ));
        }
    }
    Ok(candidates)
}

/// 将固定 Git 操作结果缓存 60 秒；首次 miss 只调用只读 argv，输入 token 不会进入远端。
async fn git_refs(
    provider: &RemoteCommandSuggestionProvider,
    query: &super::RemoteSuggestionQuery<'_>,
) -> AppResult<Option<Arc<Vec<String>>>> {
    let request = query.request;
    let host_id = query.host_id;
    let cancellation = query.cancellation;
    let operation_deadline = query.operation_deadline;
    if query.lease.is_none() {
        return Ok(None);
    }
    let key = command_cache_key_for_git(host_id, request);
    if let Some(refs) = provider.git_refs.get(&key).await {
        return Ok(Some(refs));
    }
    if provider.git_refs.is_backing_off(&key).await {
        return Ok(None);
    }
    let gate = provider.git_refs.gate(&key);
    let guard = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(kerminal_application::AppError::cancelled("Git 建议查询已取消")),
        _ = tokio::time::sleep_until(operation_deadline) => return Ok(None),
        guard = super::cache::lock_gate(gate, cancellation) => guard,
    };
    let Some(_guard) = guard else {
        return Err(kerminal_application::AppError::cancelled(
            "Git 建议查询已取消",
        ));
    };
    if let Some(refs) = provider.git_refs.get(&key).await {
        return Ok(Some(refs));
    }
    if provider.git_refs.is_backing_off(&key).await {
        return Ok(None);
    }

    let output = provider
        .execute_read_only(
            query,
            super::ReadOnlyCommand {
                source: SuggestionProviderKind::Git,
                program: "git",
                arguments: vec![
                    "for-each-ref".to_owned(),
                    "--format=%(refname:short)".to_owned(),
                    "--sort=-committerdate".to_owned(),
                    format!("--count={MAX_GIT_REFS}"),
                    "refs/heads".to_owned(),
                    "refs/remotes".to_owned(),
                    "refs/tags".to_owned(),
                ],
                working_directory: request.cwd.clone(),
            },
        )
        .await?;
    let Some(output) = output else {
        provider
            .git_refs
            .mark_failure(key, super::REMOTE_FAILURE_BACKOFF)
            .await;
        return Ok(None);
    };
    let Ok(output) = std::str::from_utf8(&output) else {
        provider
            .git_refs
            .mark_failure(key, super::REMOTE_FAILURE_BACKOFF)
            .await;
        return Ok(None);
    };
    let mut refs = output
        .lines()
        .filter(|line| !line.is_empty() && !line.chars().any(char::is_control))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    refs.dedup();
    refs.truncate(MAX_GIT_REFS);
    provider.git_refs.insert(key, refs).await;
    Ok(provider
        .git_refs
        .get(&command_cache_key_for_git(host_id, request))
        .await)
}

/// remote 名称供 fetch/pull/push 的第一参数使用，不加载凭据或展开远端 URL。
async fn git_remotes(
    provider: &RemoteCommandSuggestionProvider,
    query: &super::RemoteSuggestionQuery<'_>,
) -> AppResult<Option<Arc<Vec<String>>>> {
    let request = query.request;
    let host_id = query.host_id;
    let cancellation = query.cancellation;
    let operation_deadline = query.operation_deadline;
    if query.lease.is_none() {
        return Ok(None);
    }
    let key = command_cache_key_for_git(host_id, request);
    if let Some(remotes) = provider.git_remotes.get(&key).await {
        return Ok(Some(remotes));
    }
    if provider.git_remotes.is_backing_off(&key).await {
        return Ok(None);
    }
    let gate = provider.git_remotes.gate(&key);
    let guard = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(kerminal_application::AppError::cancelled("Git 建议查询已取消")),
        _ = tokio::time::sleep_until(operation_deadline) => return Ok(None),
        guard = super::cache::lock_gate(gate, cancellation) => guard,
    };
    let Some(_guard) = guard else {
        return Err(kerminal_application::AppError::cancelled(
            "Git 建议查询已取消",
        ));
    };
    if let Some(remotes) = provider.git_remotes.get(&key).await {
        return Ok(Some(remotes));
    }
    if provider.git_remotes.is_backing_off(&key).await {
        return Ok(None);
    }

    let output = provider
        .execute_read_only(
            query,
            super::ReadOnlyCommand {
                source: SuggestionProviderKind::Git,
                program: "git",
                arguments: vec!["remote".to_owned()],
                working_directory: request.cwd.clone(),
            },
        )
        .await?;
    let Some(output) = output else {
        provider
            .git_remotes
            .mark_failure(key, super::REMOTE_FAILURE_BACKOFF)
            .await;
        return Ok(None);
    };
    let Ok(output) = std::str::from_utf8(&output) else {
        provider
            .git_remotes
            .mark_failure(key, super::REMOTE_FAILURE_BACKOFF)
            .await;
        return Ok(None);
    };
    let mut remotes = output
        .lines()
        .filter(|line| !line.is_empty() && !line.chars().any(char::is_control))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    remotes.sort();
    remotes.dedup();
    remotes.truncate(100);
    provider.git_remotes.insert(key, remotes).await;
    Ok(provider
        .git_remotes
        .get(&command_cache_key_for_git(host_id, request))
        .await)
}

/// 安全 ASCII Git token 保持旧版裸替换；命令选项前缀不作为引用建议，其他名称走现有转义器。
///
/// @author kongweiguang
fn quote_git_reference(reference: &str, shell: ShellFamily) -> Option<String> {
    if reference.starts_with('-') {
        return None;
    }
    let legacy_safe = reference.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '/' | '_' | '-' | '.')
    });
    if legacy_safe {
        Some(reference.to_owned())
    } else {
        quote(reference, shell)
    }
}

/// 把 refs 中匹配前缀的名称绑定到用户光标位置，同时避免 shell 元字符执行。
/// 普通 ref 延续旧版裸替换；包含特殊字符的 ref 使用可信 shell 方言引用。
///
/// @author kongweiguang
fn ref_candidates(
    request: &CommandSuggestionRequest,
    prefix: &ParsedPrefix,
    host_id: &HostId,
    refs: &[String],
    typed: &str,
    shell: ShellFamily,
) -> Vec<CommandSuggestionCandidate> {
    refs.iter()
        .filter(|reference| reference.starts_with(typed))
        .filter_map(|reference| {
            let replacement = quote_git_reference(reference, shell)?;
            Some(candidate(
                super::CandidateContext {
                    request,
                    provider: SuggestionProviderKind::Git,
                    token_start: prefix.token.start,
                },
                reference.clone(),
                replacement,
                "Git 引用",
                prefix_score(typed, reference, false) - 0.05,
                &format!("{}:{}", host_id.as_str(), reference),
            ))
        })
        .collect()
}

/// 固定词表只承担常用子命令和 status 参数提示，不执行 git status 或用户输入。
fn static_candidates(
    query: &super::RemoteSuggestionQuery<'_>,
    typed: &str,
    choices: &[&str],
    description: &'static str,
    identity: &str,
) -> Vec<CommandSuggestionCandidate> {
    let request = query.request;
    let prefix = query.prefix;
    let host_id = query.host_id;
    choices
        .iter()
        .copied()
        .filter(|choice| choice.starts_with(typed))
        .filter_map(|choice| {
            let replacement = quote(choice, prefix.shell)?;
            Some(candidate(
                super::CandidateContext {
                    request,
                    provider: SuggestionProviderKind::Git,
                    token_start: prefix.token.start,
                },
                choice.to_owned(),
                replacement,
                description,
                prefix_score(typed, choice, false) - 0.02,
                &format!("{}:{identity}:{choice}", host_id.as_str()),
            ))
        })
        .collect()
}
