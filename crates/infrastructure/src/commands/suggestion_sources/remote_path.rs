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
    candidate, prefix_score,
    syntax::{ShellFamily, accepts_bare_path, command_name},
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

    let windows_path = prefix.shell == ShellFamily::PowerShell
        && request.cwd.as_deref().is_some_and(has_windows_drive_root);
    if prefix.shell == ShellFamily::PowerShell
        && ((!windows_path && has_windows_drive_prefix(&prefix.token.value))
            || (windows_path
                && (prefix.token.value.starts_with(['/', '\\'])
                    || has_windows_drive_relative_prefix(&prefix.token.value))))
    {
        return Ok(Vec::new());
    }
    let (typed_directory, basename_prefix) = split_path_prefix(&prefix.token.value, windows_path);
    let sftp_directory = if windows_path {
        typed_directory.replace('\\', "/")
    } else {
        typed_directory.to_owned()
    };
    let Some(directory) = resolve_directory(&sftp_directory, request.cwd.as_deref(), windows_path)
    else {
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

/// 仅已确认的 Windows PowerShell 路径将反斜杠视为分隔符，原输入仍供安全追加。
fn split_path_prefix(value: &str, windows_path: bool) -> (&str, &str) {
    let separator = if windows_path {
        value.rfind(['/', '\\'])
    } else {
        value.rfind('/')
    };
    match separator {
        Some(separator) => (&value[..=separator], &value[separator + 1..]),
        None => ("", value),
    }
}

/// 已转换的路径按 session cwd 查询；只有已确认的 Windows 快照才映射盘符。
fn resolve_directory(
    typed_directory: &str,
    cwd: Option<&str>,
    windows_path: bool,
) -> Option<RemotePath> {
    let value = if let Some(absolute) = sftp_absolute_path(typed_directory, windows_path) {
        absolute
    } else {
        let base = sftp_absolute_path(cwd?, windows_path)?;
        let base = RemotePath::parse(&base).ok()?;
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

/// PowerShell 的盘符也可能是自定义 PSDrive，不能仅凭输入推断远端操作系统。
fn has_windows_drive_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// 只有带根分隔符的盘符才是可映射的绝对路径，避免猜测盘符相对目录。
fn has_windows_drive_root(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3 && has_windows_drive_prefix(value) && matches!(bytes[2], b'\\' | b'/')
}

/// 盘符相对路径依赖远端进程的每盘工作目录，快照没有该信息时不应猜测。
fn has_windows_drive_relative_prefix(value: &str) -> bool {
    has_windows_drive_prefix(value) && !has_windows_drive_root(value)
}

/// POSIX 绝对路径直接使用，盘符转换仅对已确认的 Windows PowerShell 快照启用。
fn sftp_absolute_path(value: &str, windows_path: bool) -> Option<String> {
    if value.starts_with('/') {
        return Some(value.to_owned());
    }
    if !windows_path || !has_windows_drive_root(value) {
        return None;
    }
    Some(format!(
        "/{}/{}",
        &value[..2],
        value[3..].replace('\\', "/")
    ))
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
            let replacement =
                super::syntax::append_literal(&request.input, request.cursor, prefix, &completed)?;
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
