//! 解析 Git 的 NUL 分隔状态和差异格式并保留路径身份。
//!
//! @author kongweiguang

use std::collections::BTreeSet;
use std::path::Path;

use kerminal_application::configuration_sync::{
    ConfigurationSyncStatus, SyncPathStatus, SyncRepositoryState,
};
use kerminal_application::{AppError, AppResult};
use tokio_util::sync::CancellationToken;

use super::GitConfigurationSyncRepository;
use super::git::{GitCommandError, args, run_git, to_app_error};
use super::protected::{is_protected_path, missing_ignore_rules};
use super::runner::GitCommandRuntime;

/// 原始 Git 路径字节仅用于精确路径参数，离开 adapter 的只有脱敏文本。
#[derive(Clone, Debug)]
pub(super) struct GitStatusEntry {
    pub path: Vec<u8>,
    pub renamed_from: Option<Vec<u8>>,
    pub staged: bool,
    pub unstaged: bool,
    pub untracked: bool,
    pub conflicted: bool,
}

impl GitStatusEntry {
    /// 重命名的来源或目标任一受保护时，整项都不能同步。
    pub fn is_protected(&self) -> bool {
        is_protected_path(&self.path) || self.renamed_from.as_deref().is_some_and(is_protected_path)
    }

    /// 返回暂存该变更需要的原始路径，包括重命名两端。
    pub fn paths(&self) -> impl Iterator<Item = &[u8]> {
        std::iter::once(self.path.as_slice()).chain(self.renamed_from.as_deref())
    }

    /// 转换为安全展示 DTO，原始路径字节不会进入应用层。
    pub fn to_public(&self) -> SyncPathStatus {
        SyncPathStatus {
            path: display_path(&self.path),
            staged: self.staged,
            unstaged: self.unstaged,
            untracked: self.untracked,
            conflicted: self.conflicted,
        }
    }
}

/// 解析 `git status --porcelain=v1 -z`，用 NUL 分隔保留路径中的空格和换行。
pub(super) fn parse_porcelain(source: &[u8]) -> Result<Vec<GitStatusEntry>, &'static str> {
    let records = source.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut entries = Vec::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.is_empty() {
            continue;
        }
        if record.len() < 4 || record[2] != b' ' {
            return Err("unexpected Git porcelain record");
        }
        let x = record[0];
        let y = record[1];
        let path = record[3..].to_vec();
        let has_rename_source = matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C');
        let renamed_from = if has_rename_source {
            let Some(previous_path) = records.get(index).filter(|path| !path.is_empty()) else {
                return Err("Git rename record did not include its original path");
            };
            index += 1;
            Some(previous_path.to_vec())
        } else {
            None
        };
        let conflicted = matches!(
            (x, y),
            (b'D', b'D')
                | (b'A', b'U')
                | (b'U', b'D')
                | (b'U', b'A')
                | (b'D', b'U')
                | (b'A', b'A')
                | (b'U', b'U')
        );
        let untracked = x == b'?' && y == b'?';
        entries.push(GitStatusEntry {
            path,
            renamed_from,
            staged: !untracked && x != b' ',
            unstaged: !untracked && y != b' ',
            untracked,
            conflicted,
        });
    }
    Ok(entries)
}

/// 将 `git diff --name-only -z` 解析为精确字节路径以检查受保护历史。
pub(super) fn parse_name_only(source: &[u8]) -> Vec<Vec<u8>> {
    source
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| path.to_vec())
        .collect()
}

/// 构建带 NUL 分隔的字面路径参数，避免 Git 将路径解释为 glob 或相对路径逃逸。
pub(super) fn literal_top_pathspec(entries: &[GitStatusEntry]) -> Vec<u8> {
    let mut unique = BTreeSet::<Vec<u8>>::new();
    for entry in entries {
        for path in entry.paths() {
            unique.insert(path.to_vec());
        }
    }
    let prefix = b":(top,literal)";
    let mut pathspec = Vec::new();
    for path in unique {
        pathspec.extend_from_slice(prefix);
        pathspec.extend_from_slice(&path);
        pathspec.push(0);
    }
    pathspec
}

/// 将状态分成可同步项和受保护项，供界面分别展示变更与警示。
pub(super) fn split_public_status(
    entries: &[GitStatusEntry],
) -> (Vec<SyncPathStatus>, Vec<SyncPathStatus>, Vec<String>) {
    let mut local = Vec::new();
    let mut protected = Vec::new();
    let mut conflicts = Vec::new();
    for entry in entries {
        let public = entry.to_public();
        if entry.conflicted {
            conflicts.push(public.path.clone());
        }
        if entry.is_protected() {
            protected.push(public);
        } else {
            local.push(public);
        }
    }
    (local, protected, conflicts)
}

/// 去掉路径控制字符，避免伪造 UI 行或日志内容。
pub(super) fn display_path(path: &[u8]) -> String {
    String::from_utf8_lossy(path)
        .chars()
        .map(|character| {
            if character.is_control() {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

/// 通过受控阻塞线程读取 Git 状态，避免进程等待占用 Tokio 或 GPUI 线程。
pub(super) async fn status(
    repository: GitConfigurationSyncRepository,
) -> AppResult<ConfigurationSyncStatus> {
    if repository.git_runtime.state() == super::runner::GitWorkerState::PendingCleanup {
        repository
            .git_runtime
            .request_retry(tokio::time::Instant::now() + std::time::Duration::from_secs(1));
        return Ok(pending_status(&repository));
    }
    repository
        .run_blocking(
            CancellationToken::new(),
            "读取配置同步状态失败",
            |repository, _| status_blocking(repository),
        )
        .await
}

/// 在原 worker 持有 Git gate/存储锁时不并发读取仓库，只返回可见的忙碌分类。
fn pending_status(repository: &GitConfigurationSyncRepository) -> ConfigurationSyncStatus {
    let mut status = empty_status(true, repository);
    status.repository = SyncRepositoryState::PendingBusy;
    status
}

/// 收集仓库、变更、冲突和本地保护规则；先恢复共享事务并锁定本地快照，不修改工作树或远端。
fn status_blocking(
    repository: &GitConfigurationSyncRepository,
) -> AppResult<ConfigurationSyncStatus> {
    let _storage_lock = repository.lock_for_local_operation()?;
    let cancellation = CancellationToken::new();
    match run_git(
        &repository.git_runtime,
        &repository.root,
        &args(&["--version"]),
        None,
        &cancellation,
        false,
        false,
    ) {
        Ok(_) => {}
        Err(GitCommandError::Launch(source)) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(empty_status(false, repository));
        }
        Err(error) => return Err(to_app_error("读取 Git 版本失败", error)),
    };

    if !repository.root.join(".git").exists() {
        return Ok(empty_status(true, repository));
    }

    let prefix = run_git(
        &repository.git_runtime,
        &repository.root,
        &args(&["rev-parse", "--show-prefix"]),
        None,
        &cancellation,
        false,
        false,
    )
    .map_err(|error| to_app_error("确认 Git 仓库目录失败", error))?;
    if !prefix.stdout.is_empty() {
        return Err(AppError::conflict("配置目录必须是 Git 仓库根目录"));
    }

    let status_output = run_git(
        &repository.git_runtime,
        &repository.root,
        &args(&[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--renames",
        ]),
        None,
        &cancellation,
        false,
        false,
    )
    .map_err(|error| to_app_error("读取 Git 工作区状态失败", error))?;
    let entries = parse_porcelain(&status_output.stdout)
        .map_err(|_| AppError::infrastructure("解析 Git 工作区状态失败"))?;
    let (local_changes, protected_changes, conflicts) = split_public_status(&entries);
    let (branch, detached) = read_branch(&repository.git_runtime, &repository.root, &cancellation)?;
    let upstream = if branch.is_some() {
        read_upstream(&repository.git_runtime, &repository.root, &cancellation)?
    } else {
        None
    };
    let (ahead, behind) = if !detached && upstream.is_some() {
        let (ahead, behind) =
            read_ahead_behind(&repository.git_runtime, &repository.root, &cancellation)?;
        (Some(ahead), Some(behind))
    } else {
        (None, None)
    };
    let repository_state = if detached {
        SyncRepositoryState::Detached
    } else if branch.is_some() && upstream.is_none() {
        SyncRepositoryState::NoUpstream
    } else {
        SyncRepositoryState::Ready
    };
    let missing_ignore_rules = missing_ignore_rules(&repository.root)
        .map_err(|source| AppError::infrastructure_with_source("读取 Git 忽略规则失败", source))?;

    Ok(ConfigurationSyncStatus {
        git_available: true,
        repository: repository_state,
        writable: repository.recovery_state().mode
            == kerminal_domain::preferences::ConfigurationMode::ReadWrite,
        branch,
        upstream,
        ahead,
        behind,
        local_changes,
        protected_changes,
        conflicts,
        rebase_in_progress: rebase_in_progress(
            &repository.git_runtime,
            &repository.root,
            &cancellation,
        )?,
        missing_ignore_rules,
    })
}

/// 为未初始化仓库构造可显示的空状态，不创建 Git 元数据或规则文件。
fn empty_status(
    git_available: bool,
    repository: &GitConfigurationSyncRepository,
) -> ConfigurationSyncStatus {
    ConfigurationSyncStatus {
        git_available,
        repository: SyncRepositoryState::Missing,
        writable: repository.recovery_state().mode
            == kerminal_domain::preferences::ConfigurationMode::ReadWrite,
        branch: None,
        upstream: None,
        ahead: None,
        behind: None,
        local_changes: Vec::new(),
        protected_changes: Vec::new(),
        conflicts: Vec::new(),
        rebase_in_progress: false,
        missing_ignore_rules: Vec::new(),
    }
}

/// 读取当前分支并区分 detached HEAD；预期的非零状态不会被误判为进程错误。
fn read_branch(
    runner: &GitCommandRuntime,
    root: &Path,
    cancellation: &CancellationToken,
) -> AppResult<(Option<String>, bool)> {
    let output = run_git(
        runner,
        root,
        &args(&["symbolic-ref", "--quiet", "--short", "HEAD"]),
        None,
        cancellation,
        false,
        true,
    )
    .map_err(|error| to_app_error("读取 Git 分支失败", error))?;
    if output.code != Some(0) {
        return Ok((None, true));
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if branch.is_empty() {
        return Err(AppError::infrastructure("Git 返回了空分支名称"));
    }
    Ok((Some(safe_ref_name(branch)), false))
}

/// 读取分支上游 ref 名称，不暴露 remote URL 或 Git 配置正文。
fn read_upstream(
    runner: &GitCommandRuntime,
    root: &Path,
    cancellation: &CancellationToken,
) -> AppResult<Option<String>> {
    let output = run_git(
        runner,
        root,
        &args(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]),
        None,
        cancellation,
        false,
        true,
    )
    .map_err(|error| to_app_error("读取 Git 上游分支失败", error))?;
    if output.code != Some(0) {
        return Ok(None);
    }
    let upstream = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!upstream.is_empty()).then(|| safe_ref_name(upstream)))
}

/// 返回本地分支相对上游的提交数；无上游时使用受控的 conflict 分类供状态层转换。
fn read_ahead_behind(
    runner: &GitCommandRuntime,
    root: &Path,
    cancellation: &CancellationToken,
) -> AppResult<(u64, u64)> {
    let output = run_git(
        runner,
        root,
        &args(&["rev-list", "--left-right", "--count", "HEAD...@{u}"]),
        None,
        cancellation,
        false,
        true,
    )
    .map_err(|error| to_app_error("读取 Git 提交差异失败", error))?;
    if output.code != Some(0) {
        return Err(AppError::conflict("Git 尚未配置可用的上游分支"));
    }
    let counts = String::from_utf8_lossy(&output.stdout);
    let mut parts = counts.split_whitespace();
    let ahead = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| AppError::infrastructure("解析 Git ahead 计数失败"))?;
    let behind = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| AppError::infrastructure("解析 Git behind 计数失败"))?;
    Ok((ahead, behind))
}

/// 检测 Git rebase 状态目录，兼容普通仓库和 git worktree 的 git-dir 布局。
pub(super) fn rebase_in_progress(
    runner: &GitCommandRuntime,
    root: &Path,
    cancellation: &CancellationToken,
) -> AppResult<bool> {
    for name in ["rebase-merge", "rebase-apply"] {
        let output = run_git(
            runner,
            root,
            &args(&["rev-parse", "--git-path", name]),
            None,
            cancellation,
            false,
            false,
        )
        .map_err(|error| to_app_error("读取 Git 恢复状态失败", error))?;
        let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !path.is_empty() && Path::new(&path).exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 仅保留 Git ref 的安全显示字符，避免控制符伪造状态行。
fn safe_ref_name(value: String) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(256)
        .collect()
}
