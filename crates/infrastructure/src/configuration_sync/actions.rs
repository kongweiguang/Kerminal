//! Git 配置同步的显式变更动作与恢复流程。
//!
//! @author kongweiguang

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::Path;

use kerminal_application::configuration_sync::{
    ConfigurationSyncOperationKind as Kind, ConfigurationSyncOperationResult as ResultDto,
    ConfigurationSyncOutcome as Outcome,
};
use kerminal_application::{AppError, AppResult};
use tokio_util::sync::CancellationToken;

use super::GitConfigurationSyncRepository;
use super::git::{GitCommandError, GitFailureKind, args, run_git, to_app_error};
use super::gitignore::ensure_ignore_rules;
use super::protected::is_protected_path;
use super::runner::GitCommandRuntime;
use super::status::{
    GitStatusEntry, display_path, literal_top_pathspec, parse_name_only, parse_porcelain,
    rebase_in_progress, split_public_status,
};

/// 在线程池执行 fetch，避免网络握手或凭据 helper 阻塞 UI 与 Tokio worker。
pub(super) async fn fetch(
    repository: GitConfigurationSyncRepository,
    cancellation: CancellationToken,
) -> AppResult<ResultDto> {
    repository
        .run_blocking(cancellation, "执行 Git fetch 失败", fetch_blocking)
        .await
}

/// 获取远端引用但不检出文件；fetch 在只读恢复态也不会覆盖用户配置。
fn fetch_blocking(
    repository: &GitConfigurationSyncRepository,
    cancellation: &CancellationToken,
) -> AppResult<ResultDto> {
    if !repository.root.join(".git").exists() {
        return Ok(blocked(
            Kind::Fetch,
            "配置目录尚未初始化为 Git 仓库",
            Vec::new(),
        ));
    }
    match fetch_refs(&repository.git_runtime, &repository.root, cancellation) {
        Ok(()) => Ok(result(
            Kind::Fetch,
            Outcome::Succeeded,
            "已获取远端引用",
            Vec::new(),
            None,
        )),
        Err(GitCommandError::Cancelled) => Ok(cancelled(Kind::Fetch)),
        Err(error) => Err(to_app_error("获取 Git 远端引用失败", error)),
    }
}

/// 在线程池完成 fetch、受保护路径预检与本地 rebase；仅 checkout 阶段持共享存储锁。
pub(super) async fn pull(
    repository: GitConfigurationSyncRepository,
    cancellation: CancellationToken,
) -> AppResult<ResultDto> {
    repository
        .run_blocking(cancellation, "执行 Git pull 失败", pull_blocking)
        .await
}

/// 先在锁外 fetch，再在短锁区验证工作树和远端历史后执行 rebase。
fn pull_blocking(
    repository: &GitConfigurationSyncRepository,
    cancellation: &CancellationToken,
) -> AppResult<ResultDto> {
    if let Err(error) = repository.ensure_writable() {
        return Ok(blocked(Kind::Pull, error.public_message(), Vec::new()));
    }
    if !repository.root.join(".git").exists() {
        return Ok(blocked(
            Kind::Pull,
            "配置目录尚未初始化为 Git 仓库",
            Vec::new(),
        ));
    }
    if let Err(error) = fetch_refs(&repository.git_runtime, &repository.root, cancellation) {
        return match error {
            GitCommandError::Cancelled => Ok(cancelled(Kind::Pull)),
            other => Err(to_app_error("应用 Git 上游变更前获取引用失败", other)),
        };
    }

    let _storage_lock = repository.lock_for_local_operation()?;
    if rebase_in_progress(&repository.git_runtime, &repository.root, cancellation)? {
        return Ok(blocked(
            Kind::Pull,
            "已有 Git 变基需要先完成或中止",
            Vec::new(),
        ));
    }
    let entries = read_status_entries(&repository.git_runtime, &repository.root, cancellation)?;
    if !entries.is_empty() {
        let (_, _, conflicts) = split_public_status(&entries);
        return Ok(blocked(
            Kind::Pull,
            "请先提交或处理本地配置变更，再应用远端内容",
            if conflicts.is_empty() {
                changed_paths(&entries)
            } else {
                conflicts
            },
        ));
    }
    let upstream = upstream_ref(&repository.git_runtime, &repository.root, cancellation)?;
    let Some(upstream) = upstream else {
        return Ok(blocked(Kind::Pull, "当前分支尚未配置上游分支", Vec::new()));
    };
    let incoming_paths = changed_paths_in_range(
        &repository.git_runtime,
        &repository.root,
        &format!("HEAD..{upstream}"),
        cancellation,
    )?;
    let protected: Vec<String> = incoming_paths
        .into_iter()
        .filter(|path| is_protected_path(path.as_bytes()))
        .collect();
    if !protected.is_empty() {
        return Ok(blocked(
            Kind::Pull,
            "远端提交改动了本地受保护文件，已阻止检出",
            protected,
        ));
    }

    let before = read_head(&repository.git_runtime, &repository.root, cancellation)?;
    let rebase_args = args(&["rebase", "--no-autostash", &upstream]);
    match run_git(
        &repository.git_runtime,
        &repository.root,
        &rebase_args,
        None,
        cancellation,
        true,
        false,
    ) {
        Ok(_) => {
            let head = read_head(
                &repository.git_runtime,
                &repository.root,
                &CancellationToken::new(),
            )?;
            Ok(result(
                Kind::Pull,
                Outcome::Succeeded,
                "已应用远端变更",
                Vec::new(),
                head,
            ))
        }
        Err(GitCommandError::Cancelled) => {
            let check_token = CancellationToken::new();
            if rebase_in_progress(&repository.git_runtime, &repository.root, &check_token)? {
                let conflicts =
                    conflict_paths(&repository.git_runtime, &repository.root, &check_token)?;
                Ok(result(
                    Kind::Pull,
                    Outcome::Conflict,
                    "变基已取消并保留恢复状态，请处理冲突后继续或中止",
                    conflicts,
                    None,
                ))
            } else {
                let after = read_head(&repository.git_runtime, &repository.root, &check_token)?;
                if before != after {
                    Ok(result(
                        Kind::Pull,
                        Outcome::Succeeded,
                        "变基已完成",
                        Vec::new(),
                        after,
                    ))
                } else {
                    Ok(result(
                        Kind::Pull,
                        Outcome::OutcomeUnknown,
                        "变基已取消，需刷新仓库状态确认结果",
                        Vec::new(),
                        None,
                    ))
                }
            }
        }
        Err(error) if failure_kind(&error) == Some(GitFailureKind::Conflict) => {
            let conflicts = conflict_paths(
                &repository.git_runtime,
                &repository.root,
                &CancellationToken::new(),
            )?;
            Ok(result(
                Kind::Pull,
                Outcome::Conflict,
                "变基遇到冲突，请处理冲突后继续或中止",
                conflicts,
                None,
            ))
        }
        Err(error) => Err(to_app_error("应用 Git 上游变更失败", error)),
    }
}

/// 在线程池中提交显式选择的同步路径，未选择的暂存项仍留在用户索引中。
pub(super) async fn commit(
    repository: GitConfigurationSyncRepository,
    cancellation: CancellationToken,
) -> AppResult<ResultDto> {
    repository
        .run_blocking(cancellation, "执行 Git commit 失败", commit_blocking)
        .await
}

/// 只提交已枚举的同步文件；输入 EOF 未确认时不靠 HEAD 变化推断 pathspec 已完整消费。
fn commit_blocking(
    repository: &GitConfigurationSyncRepository,
    cancellation: &CancellationToken,
) -> AppResult<ResultDto> {
    if let Err(error) = repository.ensure_writable() {
        return Ok(blocked(Kind::Commit, error.public_message(), Vec::new()));
    }
    if !repository.root.join(".git").exists() {
        return Ok(blocked(
            Kind::Commit,
            "配置目录尚未初始化为 Git 仓库",
            Vec::new(),
        ));
    }
    let _storage_lock = repository.lock_for_local_operation()?;
    if rebase_in_progress(&repository.git_runtime, &repository.root, cancellation)? {
        return Ok(blocked(
            Kind::Commit,
            "Git 变基期间不能创建新提交",
            Vec::new(),
        ));
    }
    ensure_ignore_rules(&repository.root)?;
    let entries = read_status_entries(&repository.git_runtime, &repository.root, cancellation)?;
    if entries.iter().any(|entry| entry.conflicted) {
        let conflicts = conflict_paths(&repository.git_runtime, &repository.root, cancellation)?;
        return Ok(result(
            Kind::Commit,
            Outcome::Conflict,
            "工作区仍有未解决冲突，不能创建提交",
            conflicts,
            None,
        ));
    }
    let syncable: Vec<GitStatusEntry> = entries
        .into_iter()
        .filter(|entry| !entry.is_protected())
        .collect();
    if syncable.is_empty() {
        return Ok(result(
            Kind::Commit,
            Outcome::NoChanges,
            "没有可同步的本地变更",
            Vec::new(),
            None,
        ));
    }
    let paths = changed_paths(&syncable);
    let pathspec = literal_top_pathspec(&syncable);
    match run_git(
        &repository.git_runtime,
        &repository.root,
        &args(&[
            "add",
            "--all",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ]),
        Some(pathspec.clone()),
        cancellation,
        false,
        false,
    ) {
        Ok(_) => {}
        Err(GitCommandError::Cancelled) => return Ok(cancelled(Kind::Commit)),
        Err(error) => return Err(to_app_error("暂存配置同步文件失败", error)),
    }
    let staged = run_git(
        &repository.git_runtime,
        &repository.root,
        &args(&["diff", "--cached", "--name-only", "-z"]),
        None,
        cancellation,
        false,
        false,
    )
    .map_err(|error| to_app_error("读取暂存变更失败", error))?;
    let staged_paths = parse_name_only(&staged.stdout);
    if !staged_paths.iter().any(|path| !is_protected_path(path)) {
        return Ok(result(
            Kind::Commit,
            Outcome::NoChanges,
            "没有可提交的同步变更",
            Vec::new(),
            None,
        ));
    }

    let before = read_head(&repository.git_runtime, &repository.root, cancellation)?;
    let commit_args = args(&[
        "commit",
        "--only",
        "--pathspec-from-file=-",
        "--pathspec-file-nul",
        "-m",
        "Sync Kerminal configuration",
    ]);
    let commit_result = run_git(
        &repository.git_runtime,
        &repository.root,
        &commit_args,
        Some(pathspec),
        cancellation,
        true,
        false,
    );
    match commit_result {
        Ok(_) => {
            let head = read_head(
                &repository.git_runtime,
                &repository.root,
                &CancellationToken::new(),
            )?;
            Ok(result(
                Kind::Commit,
                Outcome::Succeeded,
                "已创建本地提交",
                paths,
                head,
            ))
        }
        Err(error) => {
            if matches!(&error, GitCommandError::OutcomeUnknown { .. }) {
                // stdin EOF 未确认时，HEAD 变化只能证明有提交，不能证明 pathspec 已完整送达。
                return Ok(result(
                    Kind::Commit,
                    Outcome::OutcomeUnknown,
                    "提交结果未知，请刷新仓库状态确认后再操作",
                    paths,
                    None,
                ));
            }
            let cancelled = matches!(&error, GitCommandError::Cancelled);
            let check_token = CancellationToken::new();
            let after = read_head(&repository.git_runtime, &repository.root, &check_token)?;
            if after != before {
                match commit_parent(&repository.git_runtime, &repository.root, &check_token)? {
                    Some(parent) if before.as_deref() == Some(parent.as_str()) => Ok(result(
                        Kind::Commit,
                        Outcome::Succeeded,
                        "已创建本地提交",
                        paths,
                        after,
                    )),
                    None if before.is_none() => Ok(result(
                        Kind::Commit,
                        Outcome::Succeeded,
                        "已创建本地提交",
                        paths,
                        after,
                    )),
                    _ => Ok(result(
                        Kind::Commit,
                        Outcome::OutcomeUnknown,
                        "提交结果未知，请刷新仓库状态确认后再操作",
                        paths,
                        None,
                    )),
                }
            } else if cancelled {
                Ok(cancelled_result(Kind::Commit, paths))
            } else {
                Err(to_app_error("创建 Git 本地提交失败", error))
            }
        }
    }
}

/// 在线程池预检即将推送的全部提交，再在线程池外的锁区之外执行远端 push。
pub(super) async fn push(
    repository: GitConfigurationSyncRepository,
    cancellation: CancellationToken,
) -> AppResult<ResultDto> {
    repository
        .run_blocking(cancellation, "执行 Git push 失败", push_blocking)
        .await
}

/// 本地锁只覆盖分支和提交历史预检；远端网络过程不持有配置根锁。
fn push_blocking(
    repository: &GitConfigurationSyncRepository,
    cancellation: &CancellationToken,
) -> AppResult<ResultDto> {
    if let Err(error) = repository.ensure_writable() {
        return Ok(blocked(Kind::Push, error.public_message(), Vec::new()));
    }
    if !repository.root.join(".git").exists() {
        return Ok(blocked(
            Kind::Push,
            "配置目录尚未初始化为 Git 仓库",
            Vec::new(),
        ));
    }
    let (remote, refspec, outgoing_paths, head) = {
        let _storage_lock = repository.lock_for_local_operation()?;
        if rebase_in_progress(&repository.git_runtime, &repository.root, cancellation)? {
            return Ok(blocked(Kind::Push, "Git 变基期间不能推送提交", Vec::new()));
        }
        let entries = read_status_entries(&repository.git_runtime, &repository.root, cancellation)?;
        if entries.iter().any(|entry| entry.conflicted) {
            return Ok(result(
                Kind::Push,
                Outcome::Conflict,
                "工作区仍有未解决冲突，不能推送",
                conflict_paths(&repository.git_runtime, &repository.root, cancellation)?,
                None,
            ));
        }
        let (local, _, _) = split_public_status(&entries);
        if !local.is_empty() {
            return Ok(blocked(
                Kind::Push,
                "请先提交可同步的本地变更，再执行推送",
                local.into_iter().map(|entry| entry.path).collect(),
            ));
        }
        let Some(branch) = current_branch(&repository.git_runtime, &repository.root, cancellation)?
        else {
            return Ok(blocked(
                Kind::Push,
                "当前处于 detached HEAD，不能推送",
                Vec::new(),
            ));
        };
        let Some(upstream) = upstream_ref(&repository.git_runtime, &repository.root, cancellation)?
        else {
            return Ok(blocked(Kind::Push, "当前分支尚未配置上游分支", Vec::new()));
        };
        if commit_count_in_range(
            &repository.git_runtime,
            &repository.root,
            &format!("{upstream}..HEAD"),
            cancellation,
        )? == 0
        {
            return Ok(result(
                Kind::Push,
                Outcome::NoChanges,
                "没有待推送的本地提交",
                Vec::new(),
                read_head(&repository.git_runtime, &repository.root, cancellation)?,
            ));
        }
        let (remote, branch_ref) = configured_push_target(
            &repository.git_runtime,
            &repository.root,
            &branch,
            cancellation,
        )?;
        let outgoing_paths = changed_paths_in_range(
            &repository.git_runtime,
            &repository.root,
            &format!("{upstream}..HEAD"),
            cancellation,
        )?;
        let protected: Vec<String> = outgoing_paths
            .iter()
            .filter(|path| is_protected_path(path.as_bytes()))
            .cloned()
            .collect();
        if !protected.is_empty() {
            return Ok(blocked(
                Kind::Push,
                "待推送提交改动了本地受保护文件，已阻止推送",
                protected,
            ));
        }
        let head = read_head(&repository.git_runtime, &repository.root, cancellation)?;
        let Some(head) = head else {
            return Ok(blocked(Kind::Push, "当前分支没有可推送的提交", Vec::new()));
        };
        (remote, format!("{head}:{branch_ref}"), outgoing_paths, head)
    };

    let push_args = vec![
        OsString::from("push"),
        OsString::from("--porcelain"),
        OsString::from(remote),
        OsString::from(refspec),
    ];
    match run_git(
        &repository.git_runtime,
        &repository.root,
        &push_args,
        None,
        cancellation,
        false,
        false,
    ) {
        Ok(_) => Ok(result(
            Kind::Push,
            Outcome::Succeeded,
            "本地提交已推送到远端",
            outgoing_paths,
            Some(head),
        )),
        Err(GitCommandError::Cancelled) => Ok(result(
            Kind::Push,
            Outcome::OutcomeUnknown,
            "推送已取消，远端可能已接收提交；请刷新状态确认",
            outgoing_paths,
            Some(head),
        )),
        Err(error) if failure_kind(&error) == Some(GitFailureKind::Conflict) => Ok(result(
            Kind::Push,
            Outcome::Conflict,
            "远端分支已变化或拒绝该提交，请先获取并处理远端变更",
            outgoing_paths,
            Some(head),
        )),
        Err(error) if failure_kind(&error) == Some(GitFailureKind::Network) => Ok(result(
            Kind::Push,
            Outcome::OutcomeUnknown,
            "远端网络中断，无法确认提交是否已接收；请刷新状态确认",
            outgoing_paths,
            Some(head),
        )),
        Err(error)
            if !matches!(
                failure_kind(&error),
                Some(GitFailureKind::Authentication)
                    | Some(GitFailureKind::Conflict)
                    | Some(GitFailureKind::MissingUpstream)
                    | Some(GitFailureKind::InvalidRepository)
            ) && !matches!(&error, GitCommandError::Launch(_)) =>
        {
            Ok(result(
                Kind::Push,
                Outcome::OutcomeUnknown,
                "推送结果未知，请刷新仓库状态确认远端是否已接收",
                outgoing_paths,
                Some(head),
            ))
        }
        Err(error) => Err(to_app_error("推送 Git 本地提交失败", error)),
    }
}

/// 在线程池继续用户已解决冲突的 rebase，并在执行前再次验证受保护路径。
pub(super) async fn continue_rebase(
    repository: GitConfigurationSyncRepository,
    cancellation: CancellationToken,
) -> AppResult<ResultDto> {
    repository
        .run_blocking(cancellation, "继续 Git 变基失败", continue_rebase_blocking)
        .await
}

/// 只有 rebase 存在、受保护文件未改动且冲突已解决时才继续。
fn continue_rebase_blocking(
    repository: &GitConfigurationSyncRepository,
    cancellation: &CancellationToken,
) -> AppResult<ResultDto> {
    if let Err(error) = repository.ensure_writable() {
        return Ok(blocked(
            Kind::ContinueRebase,
            error.public_message(),
            Vec::new(),
        ));
    }
    let _storage_lock = repository.lock_for_local_operation()?;
    if !rebase_in_progress(&repository.git_runtime, &repository.root, cancellation)? {
        return Ok(blocked(
            Kind::ContinueRebase,
            "当前没有待继续的 Git 变基",
            Vec::new(),
        ));
    }
    let entries = read_status_entries(&repository.git_runtime, &repository.root, cancellation)?;
    let protected: Vec<String> = entries
        .iter()
        .filter(|entry| entry.is_protected())
        .flat_map(|entry| entry.paths().map(display_path))
        .collect();
    if !protected.is_empty() {
        return Ok(blocked(
            Kind::ContinueRebase,
            "变基恢复期间发现本地受保护文件变更，已暂停恢复",
            sorted_unique(protected),
        ));
    }
    let conflicts = conflict_paths(&repository.git_runtime, &repository.root, cancellation)?;
    if !conflicts.is_empty() {
        return Ok(result(
            Kind::ContinueRebase,
            Outcome::Conflict,
            "请先解决所有报告的冲突，再继续变基",
            conflicts,
            None,
        ));
    }
    match run_git(
        &repository.git_runtime,
        &repository.root,
        &args(&["rebase", "--continue"]),
        None,
        cancellation,
        true,
        false,
    ) {
        Ok(_) => Ok(result(
            Kind::ContinueRebase,
            Outcome::Succeeded,
            "已继续并完成变基",
            Vec::new(),
            read_head(
                &repository.git_runtime,
                &repository.root,
                &CancellationToken::new(),
            )?,
        )),
        Err(GitCommandError::Cancelled) => {
            let check_token = CancellationToken::new();
            if rebase_in_progress(&repository.git_runtime, &repository.root, &check_token)? {
                Ok(result(
                    Kind::ContinueRebase,
                    Outcome::Conflict,
                    "继续变基已取消，恢复状态仍保留",
                    conflict_paths(&repository.git_runtime, &repository.root, &check_token)?,
                    None,
                ))
            } else {
                Ok(result(
                    Kind::ContinueRebase,
                    Outcome::OutcomeUnknown,
                    "继续变基已取消，需刷新仓库状态确认结果",
                    Vec::new(),
                    None,
                ))
            }
        }
        Err(error) if failure_kind(&error) == Some(GitFailureKind::Conflict) => Ok(result(
            Kind::ContinueRebase,
            Outcome::Conflict,
            "继续变基仍有冲突，请处理后再次继续或中止",
            conflict_paths(
                &repository.git_runtime,
                &repository.root,
                &CancellationToken::new(),
            )?,
            None,
        )),
        Err(error) => Err(to_app_error("继续 Git 变基失败", error)),
    }
}

/// 在线程池中检查并中止 rebase；受保护文件被触碰时拒绝 Git 回滚。
pub(super) async fn abort_rebase(
    repository: GitConfigurationSyncRepository,
    cancellation: CancellationToken,
) -> AppResult<ResultDto> {
    repository
        .run_blocking(cancellation, "中止 Git 变基失败", abort_rebase_blocking)
        .await
}

/// 仅在工作树未修改本地保护路径时执行 Git rebase --abort。
fn abort_rebase_blocking(
    repository: &GitConfigurationSyncRepository,
    cancellation: &CancellationToken,
) -> AppResult<ResultDto> {
    if let Err(error) = repository.ensure_writable() {
        return Ok(blocked(
            Kind::AbortRebase,
            error.public_message(),
            Vec::new(),
        ));
    }
    let _storage_lock = repository.lock_for_local_operation()?;
    if !rebase_in_progress(&repository.git_runtime, &repository.root, cancellation)? {
        return Ok(blocked(
            Kind::AbortRebase,
            "当前没有待中止的 Git 变基",
            Vec::new(),
        ));
    }
    let entries = read_status_entries(&repository.git_runtime, &repository.root, cancellation)?;
    let protected: Vec<String> = entries
        .iter()
        .filter(|entry| entry.is_protected())
        .flat_map(|entry| entry.paths().map(display_path))
        .collect();
    if !protected.is_empty() {
        return Ok(blocked(
            Kind::AbortRebase,
            "变基恢复期间发现本地受保护文件变更，已暂停中止",
            sorted_unique(protected),
        ));
    }
    let expected_head = read_git_text(
        &repository.git_runtime,
        &repository.root,
        &["rev-parse", "--verify", "ORIG_HEAD"],
        cancellation,
        true,
    )?;
    match run_git(
        &repository.git_runtime,
        &repository.root,
        &args(&["rebase", "--abort"]),
        None,
        cancellation,
        false,
        false,
    ) {
        Ok(_) => Ok(result(
            Kind::AbortRebase,
            Outcome::Succeeded,
            "已恢复变基前的本地分支状态",
            Vec::new(),
            read_head(
                &repository.git_runtime,
                &repository.root,
                &CancellationToken::new(),
            )?,
        )),
        Err(GitCommandError::Cancelled) => {
            let check_token = CancellationToken::new();
            if !rebase_in_progress(&repository.git_runtime, &repository.root, &check_token)? {
                let current = read_head(&repository.git_runtime, &repository.root, &check_token)?;
                if current.as_deref() == expected_head.as_deref() {
                    return Ok(result(
                        Kind::AbortRebase,
                        Outcome::Succeeded,
                        "已恢复变基前的本地分支状态",
                        Vec::new(),
                        current,
                    ));
                }
            }
            Ok(result(
                Kind::AbortRebase,
                Outcome::OutcomeUnknown,
                "中止变基已取消，需刷新状态确认恢复结果",
                Vec::new(),
                None,
            ))
        }
        Err(error) => Err(to_app_error("中止 Git 变基失败", error)),
    }
}

/// 获取默认远端的 fetch 引用；本操作只改 Git 引用，不检出配置文件。
fn fetch_refs(
    runner: &GitCommandRuntime,
    root: &Path,
    cancellation: &CancellationToken,
) -> Result<(), GitCommandError> {
    run_git(
        runner,
        root,
        &args(&["fetch", "--no-tags"]),
        None,
        cancellation,
        false,
        false,
    )?;
    Ok(())
}

/// 读取零分隔工作树状态，防止空格、换行和非 ASCII 文件名被拆错。
fn read_status_entries(
    runner: &GitCommandRuntime,
    root: &Path,
    cancellation: &CancellationToken,
) -> AppResult<Vec<GitStatusEntry>> {
    let output = run_git(
        runner,
        root,
        &args(&[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--renames",
        ]),
        None,
        cancellation,
        false,
        false,
    )
    .map_err(|error| to_app_error("读取 Git 工作区状态失败", error))?;
    parse_porcelain(&output.stdout).map_err(|_| AppError::infrastructure("解析 Git 工作区状态失败"))
}

/// 返回当前 symbolic branch 名称；detached HEAD 由 None 表示。
fn current_branch(
    runner: &GitCommandRuntime,
    root: &Path,
    cancellation: &CancellationToken,
) -> AppResult<Option<String>> {
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
        return Ok(None);
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!branch.is_empty()).then_some(branch))
}

/// 读取当前上游 ref 的短名称，允许无上游分支作为正常可显示状态。
fn upstream_ref(
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
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!value.is_empty()).then_some(value))
}

/// 从 Git 配置读取显式 push 目标，并拒绝 URL、选项和不符合 heads 命名的目标。
fn configured_push_target(
    runner: &GitCommandRuntime,
    root: &Path,
    branch: &str,
    cancellation: &CancellationToken,
) -> AppResult<(String, String)> {
    let remote_key = format!("branch.{branch}.remote");
    let merge_key = format!("branch.{branch}.merge");
    let remote = read_git_text_owned(runner, root, &remote_key, cancellation)?;
    let merge = read_git_text_owned(runner, root, &merge_key, cancellation)?;
    let safe_remote = !remote.is_empty()
        && !remote.starts_with('-')
        && !remote.contains("://")
        && !remote.contains('@')
        && remote.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-' | '/')
        });
    let branch_ref = merge
        .strip_prefix("refs/heads/")
        .filter(|name| !name.is_empty() && !name.starts_with('-'))
        .ok_or_else(|| AppError::conflict("上游分支目标格式无效，已阻止推送"))?;
    if !safe_remote || remote == "." {
        return Err(AppError::conflict("当前上游不是可安全推送的远端名称"));
    }
    Ok((remote, format!("refs/heads/{branch_ref}")))
}

/// 检查指定提交区间里每一个 commit 的改动，避免保护路径先改后还原绕过过滤。
fn changed_paths_in_range(
    runner: &GitCommandRuntime,
    root: &Path,
    revision_range: &str,
    cancellation: &CancellationToken,
) -> AppResult<Vec<String>> {
    let commits = run_git(
        runner,
        root,
        &args(&["rev-list", revision_range]),
        None,
        cancellation,
        false,
        false,
    )
    .map_err(|error| to_app_error("检查 Git 提交历史失败", error))?;
    let mut protected = Vec::new();
    for commit in String::from_utf8_lossy(&commits.stdout).lines() {
        if commit.is_empty() {
            continue;
        }
        let command = vec![
            OsString::from("diff-tree"),
            OsString::from("--root"),
            OsString::from("--no-commit-id"),
            OsString::from("--name-only"),
            OsString::from("-r"),
            OsString::from("-m"),
            OsString::from("-z"),
            OsString::from(commit),
        ];
        // 路径参数来自 Git 生成的 commit id，固定 SHA 格式且不含用户输入。
        let output = run_git(runner, root, &command, None, cancellation, false, false)
            .map_err(|error| to_app_error("检查 Git 提交文件列表失败", error))?;
        for path in parse_name_only(&output.stdout) {
            protected.push(display_path(&path));
        }
    }
    Ok(sorted_unique(protected))
}

/// 读取提交区间的提交数，以免把已同步状态报告成一次成功推送。
fn commit_count_in_range(
    runner: &GitCommandRuntime,
    root: &Path,
    revision_range: &str,
    cancellation: &CancellationToken,
) -> AppResult<u64> {
    let output = run_git(
        runner,
        root,
        &[
            OsString::from("rev-list"),
            OsString::from("--count"),
            OsString::from(revision_range),
        ],
        None,
        cancellation,
        false,
        false,
    )
    .map_err(|error| to_app_error("读取待推送提交数失败", error))?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .map_err(|_| AppError::infrastructure("解析待推送提交数失败"))
}

/// 读取 HEAD commit hash；新仓库尚无首提交时返回 None。
fn read_head(
    runner: &GitCommandRuntime,
    root: &Path,
    cancellation: &CancellationToken,
) -> AppResult<Option<String>> {
    let value = read_git_text(
        runner,
        root,
        &["rev-parse", "--verify", "HEAD^{commit}"],
        cancellation,
        true,
    )?;
    Ok(value)
}

/// 读取当前 HEAD 的唯一父提交，用于验证被取消的 commit 是否已经确定写入。
fn commit_parent(
    runner: &GitCommandRuntime,
    root: &Path,
    cancellation: &CancellationToken,
) -> AppResult<Option<String>> {
    let Some(line) = read_git_text(
        runner,
        root,
        &["rev-list", "--parents", "-n", "1", "HEAD"],
        cancellation,
        true,
    )?
    else {
        return Ok(None);
    };
    Ok(line.split_whitespace().nth(1).map(str::to_owned))
}

/// 执行只读 Git 查询并返回第一行文本；允许调用方显式接受预期的非零状态。
fn read_git_text(
    runner: &GitCommandRuntime,
    root: &Path,
    arguments: &[&str],
    cancellation: &CancellationToken,
    allow_nonzero: bool,
) -> AppResult<Option<String>> {
    let output = run_git(
        runner,
        root,
        &args(arguments),
        None,
        cancellation,
        false,
        allow_nonzero,
    )
    .map_err(|error| to_app_error("读取 Git 状态失败", error))?;
    if output.code != Some(0) {
        return Ok(None);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned))
}

/// 使用动态 config key 查询受控本地 Git 配置，不展示读取值给 UI。
fn read_git_text_owned(
    runner: &GitCommandRuntime,
    root: &Path,
    key: &str,
    cancellation: &CancellationToken,
) -> AppResult<String> {
    let output = run_git(
        runner,
        root,
        &[
            OsString::from("config"),
            OsString::from("--get"),
            OsString::from(key),
        ],
        None,
        cancellation,
        false,
        true,
    )
    .map_err(|error| to_app_error("读取 Git 推送目标失败", error))?;
    if output.code != Some(0) {
        return Err(AppError::conflict("当前分支尚未配置完整的推送目标"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// 返回当前未解决冲突的脱敏路径列表。
fn conflict_paths(
    runner: &GitCommandRuntime,
    root: &Path,
    cancellation: &CancellationToken,
) -> AppResult<Vec<String>> {
    let entries = read_status_entries(runner, root, cancellation)?;
    let (_, _, conflicts) = split_public_status(&entries);
    Ok(sorted_unique(conflicts))
}

/// 汇总状态项的路径并保留 rename 两端，使 UI 能说明本次操作涉及的文件。
fn changed_paths(entries: &[GitStatusEntry]) -> Vec<String> {
    sorted_unique(
        entries
            .iter()
            .flat_map(|entry| entry.paths().map(display_path))
            .collect(),
    )
}

/// 结果对象只携带安全摘要和相对路径，不回传 Git 输出或远端信息。
fn result(
    kind: Kind,
    outcome: Outcome,
    summary: &str,
    affected_paths: Vec<String>,
    commit_hash: Option<String>,
) -> ResultDto {
    ResultDto {
        kind,
        outcome,
        summary: summary.to_owned(),
        affected_paths,
        commit_hash,
    }
}

/// 表示用户可重新触发的取消，不把部分阶段性工作伪装成成功。
fn cancelled(kind: Kind) -> ResultDto {
    result(kind, Outcome::Cancelled, "操作已取消", Vec::new(), None)
}

/// 在已经枚举路径时保留受影响项，便于 UI 显示取消前的具体提交范围。
fn cancelled_result(kind: Kind, paths: Vec<String>) -> ResultDto {
    result(
        kind,
        Outcome::Cancelled,
        "操作已取消，未创建本地提交",
        paths,
        None,
    )
}

/// 将前置条件拒绝明确表示为 blocked，而非把保护策略误报成 Git 故障。
fn blocked(kind: Kind, summary: &str, paths: Vec<String>) -> ResultDto {
    result(kind, Outcome::Blocked, summary, paths, None)
}

/// 读取 Git 子进程的分类以决定返回冲突、未知结果还是底层脱敏错误。
fn failure_kind(error: &GitCommandError) -> Option<GitFailureKind> {
    match error {
        GitCommandError::Failed { kind, .. } => Some(*kind),
        _ => None,
    }
}

/// 对路径集合排序去重，确保多个 merge parent 不产生重复 UI 行。
fn sorted_unique(paths: Vec<String>) -> Vec<String> {
    let unique: BTreeSet<String> = paths.into_iter().collect();
    unique.into_iter().collect()
}
