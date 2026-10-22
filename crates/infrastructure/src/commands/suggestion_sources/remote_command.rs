//! SSH 可执行命令建议来自固定 PATH 查询与一层 SFTP 目录项。
//!
//! @author kongweiguang

use std::{collections::HashSet, sync::Arc};

use kerminal_application::AppResult;
use kerminal_domain::{
    commands::{CommandSuggestionCandidate, SuggestionProviderKind},
    files::FileEntryKind,
};
use tokio::time::Instant;

use super::{
    CandidateContext, MAX_COMMAND_ENTRIES, MAX_PATH_DIRECTORIES, RemoteCommandSuggestionProvider,
    RemoteSuggestionQuery, candidate, prefix_score, quote,
    syntax::{ParsedPrefix, ShellFamily, is_command_position},
};

const POSIX_PATH_PROBE: &str = "printenv";
const POWERSHELL_PATH_PROBE: &str = "Get-Item";
const POWERSHELL_PATH_ARGUMENT: &str = "Env:Path";
const POSIX_BUILTINS: &[&str] = &[
    "alias", "bg", "break", "cd", "command", "continue", "echo", "eval", "exec", "exit", "export",
    "false", "fg", "getopts", "hash", "jobs", "kill", "printf", "pwd", "read", "readonly",
    "return", "set", "shift", "source", "test", "times", "trap", "true", "type", "ulimit", "umask",
    "unalias", "unset", "wait",
];

/// 只在有待补全的裸命令名时请求可信 SSH shell kind，空前缀不扫描整个 PATH。
pub(super) fn requires_remote_query(prefix: &ParsedPrefix) -> bool {
    is_command_position(prefix)
        && !prefix.token.value.is_empty()
        && !prefix.token.value.contains('/')
        && !prefix.token.value.contains('\\')
}

/// 只在首个 shell token 上补全可执行名，避免把命令参数误作 PATH 查询。
pub(super) async fn suggestions(
    provider: &RemoteCommandSuggestionProvider,
    query: &RemoteSuggestionQuery<'_>,
) -> AppResult<Vec<CommandSuggestionCandidate>> {
    let request = query.request;
    let prefix = query.prefix;
    let host_id = query.host_id;
    let Some(lease) = query.lease else {
        return Ok(Vec::new());
    };
    if !matches!(
        lease.shell_kind(),
        kerminal_application::connections::RemoteShellKind::Bash
            | kerminal_application::connections::RemoteShellKind::Zsh
            | kerminal_application::connections::RemoteShellKind::Fish
            | kerminal_application::connections::RemoteShellKind::PowerShell
    ) || lease.shell_dialect() == kerminal_domain::connections::RemoteShellDialect::Unknown
        || !is_command_position(prefix)
    {
        return Ok(Vec::new());
    }
    let typed = &prefix.token.value;
    if typed.contains('/') || typed.contains('\\') {
        return Ok(Vec::new());
    }
    let names = executable_names(provider, query).await?;
    let Some(names) = names else {
        return Ok(Vec::new());
    };
    let mut candidates = names
        .iter()
        .filter(|name| name.starts_with(typed))
        .filter_map(|name| {
            let replacement = quote(name, prefix.shell)?;
            let score = prefix_score(typed, name, false) - 0.1;
            Some(candidate(
                CandidateContext {
                    request,
                    provider: SuggestionProviderKind::RemoteCommand,
                    token_start: prefix.token.start,
                },
                name.clone(),
                replacement,
                "远端可执行命令",
                score,
                &format!("{}:{}", host_id.as_str(), name),
            ))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.display_text.cmp(&right.display_text))
    });
    candidates.truncate(MAX_COMMAND_ENTRIES.min(100));
    Ok(candidates)
}

/// 通过一个固定 argv 读取 PATH，再最多枚举十二个目录且只接受明确可执行文件属性。
async fn executable_names(
    provider: &RemoteCommandSuggestionProvider,
    query: &RemoteSuggestionQuery<'_>,
) -> AppResult<Option<Arc<Vec<String>>>> {
    let request = query.request;
    let shell = query.prefix.shell;
    let host_id = query.host_id;
    let cancellation = query.cancellation;
    let operation_deadline = query.operation_deadline;
    if query.lease.is_none() {
        return Ok(None);
    }
    let key = RemoteCommandSuggestionProvider::command_cache_key(host_id, request);
    if let Some(names) = provider.executables.get(&key).await {
        return Ok(Some(names));
    }
    if provider.executables.is_backing_off(&key).await {
        return Ok(None);
    }
    let gate = provider.executables.gate(&key);
    let guard = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(kerminal_application::AppError::cancelled("远端命令建议已取消")),
            _ = tokio::time::sleep_until(operation_deadline) => return Ok(None),
        guard = super::cache::lock_gate(gate, cancellation) => guard,
    };
    let Some(_guard) = guard else {
        return Err(kerminal_application::AppError::cancelled(
            "远端命令建议已取消",
        ));
    };
    if let Some(names) = provider.executables.get(&key).await {
        return Ok(Some(names));
    }
    if provider.executables.is_backing_off(&key).await {
        return Ok(None);
    }

    let (program, arguments) = match shell {
        ShellFamily::Posix => (POSIX_PATH_PROBE, vec!["PATH".to_owned()]),
        ShellFamily::PowerShell => (
            POWERSHELL_PATH_PROBE,
            vec![POWERSHELL_PATH_ARGUMENT.to_owned()],
        ),
        ShellFamily::Unknown => return Ok(None),
    };
    let path_output = provider
        .execute_read_only(
            query,
            super::ReadOnlyCommand {
                source: SuggestionProviderKind::RemoteCommand,
                program,
                arguments,
                working_directory: request.cwd.clone(),
            },
        )
        .await?;
    let Some(path_output) = path_output else {
        provider
            .executables
            .mark_failure(key, super::REMOTE_FAILURE_BACKOFF)
            .await;
        return Ok(None);
    };
    let Some(path_line) = path_value(&path_output, shell) else {
        provider
            .executables
            .mark_failure(key, super::REMOTE_FAILURE_BACKOFF)
            .await;
        return Ok(None);
    };
    if path_line.len() > 8 * 1024 || path_line.chars().any(char::is_control) {
        provider
            .executables
            .mark_failure(key, super::REMOTE_FAILURE_BACKOFF)
            .await;
        return Ok(None);
    }

    let mut directories = Vec::new();
    for path in path_directories(&path_line, shell) {
        if !directories
            .iter()
            .any(|existing: &kerminal_domain::files::RemotePath| existing == &path)
        {
            directories.push(path);
        }
        if directories.len() >= MAX_PATH_DIRECTORIES {
            break;
        }
    }

    let mut names = HashSet::new();
    if shell == ShellFamily::Posix {
        names.extend(POSIX_BUILTINS.iter().map(|name| (*name).to_owned()));
    }
    let mut completed_all_directories = true;
    for directory in directories {
        if cancellation.is_cancelled() {
            return Err(kerminal_application::AppError::cancelled(
                "远端命令建议已取消",
            ));
        }
        if Instant::now() >= operation_deadline {
            completed_all_directories = false;
            break;
        }
        let entries = provider
            .directory_entries(
                SuggestionProviderKind::RemoteCommand,
                host_id,
                &directory,
                cancellation,
                operation_deadline,
            )
            .await?;
        let Some(entries) = entries else {
            completed_all_directories = false;
            continue;
        };
        names.extend(
            entries
                .iter()
                .filter(|entry| {
                    matches!(entry.kind, FileEntryKind::File | FileEntryKind::Symlink)
                        && entry.executable
                })
                .map(|entry| entry.name.clone()),
        );
    }

    let mut names = names.into_iter().collect::<Vec<_>>();
    names.sort();
    names.truncate(MAX_COMMAND_ENTRIES);
    if completed_all_directories {
        provider
            .executables
            .insert(key.clone(), names.clone())
            .await;
        Ok(provider.executables.get(&key).await)
    } else if names.is_empty() {
        provider
            .executables
            .mark_failure(key, super::REMOTE_FAILURE_BACKOFF)
            .await;
        Ok(None)
    } else {
        // 总预算耗尽时只把本次已有结果用于响应，不将部分 PATH 快照缓存成完整结果。
        Ok(Some(Arc::new(names)))
    }
}

/// 只接受无截断的可信 shell PATH 输出；PowerShell 的格式化表格需按名称列提取值列。
fn path_value(output: &[u8], shell: ShellFamily) -> Option<String> {
    let text = std::str::from_utf8(output).ok()?;
    let value = match shell {
        ShellFamily::Posix => text.lines().next()?.trim_end_matches('\r').to_owned(),
        ShellFamily::PowerShell => text.lines().find_map(|line| {
            let line = line.trim_start();
            let (name, value) = line.split_once(char::is_whitespace)?;
            name.eq_ignore_ascii_case("Path")
                .then(|| value.trim().trim_end_matches('\r').to_owned())
        })?,
        ShellFamily::Unknown => return None,
    };
    if value.is_empty() || value.ends_with("...") || value.ends_with('…') {
        return None;
    }
    Some(value)
}

/// 将 PATH 项限制为可由 POSIX SFTP RemotePath 表示的绝对路径，并跳过相对或未展开变量。
fn path_directories(value: &str, shell: ShellFamily) -> Vec<kerminal_domain::files::RemotePath> {
    let delimiter = match shell {
        ShellFamily::Posix => ':',
        ShellFamily::PowerShell if value.contains(';') || looks_like_windows_path(value) => ';',
        ShellFamily::PowerShell => ':',
        ShellFamily::Unknown => return Vec::new(),
    };
    value
        .split(delimiter)
        .filter_map(|entry| path_directory(entry, shell))
        .take(MAX_PATH_DIRECTORIES)
        .collect()
}

/// Windows drive 路径映射为 OpenSSH SFTP 常用的 `/C:/...` 形式，UNC 和相对路径保持关闭。
fn path_directory(entry: &str, shell: ShellFamily) -> Option<kerminal_domain::files::RemotePath> {
    let entry = entry.trim();
    if entry.is_empty() || entry.chars().any(char::is_control) {
        return None;
    }
    if entry.starts_with('/') {
        return super::parse_remote_path(entry);
    }
    if shell != ShellFamily::PowerShell
        || entry.starts_with('\\')
        || entry.starts_with('%')
        || entry.starts_with('$')
        || entry.starts_with('~')
    {
        return None;
    }
    let bytes = entry.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/')
    {
        return None;
    }
    let tail = entry[3..].replace('\\', "/");
    let normalized = if tail.is_empty() {
        format!("/{}/", &entry[..2])
    } else {
        format!("/{}/{}", &entry[..2], tail)
    };
    super::parse_remote_path(&normalized)
}

/// 只有在字符串开头确为 drive-letter 绝对路径时才将 PATH 分隔符解释为 Windows 分号。
fn looks_like_windows_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}
