//! 容器下载到本机后的安全逐项合并与原子文件提交。
//!
//! @author kongweiguang

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    fs, io,
    path::{Path, PathBuf},
};

use kerminal_domain::files::{ConflictPolicy, TransferKind};

use crate::storage::{AtomicCommitError, CommitMode, commit_staged_file};

/// 一项已经完成类型和冲突预检的本机提交计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalCommitAction {
    CreateDirectory {
        destination: PathBuf,
    },
    ReplaceFile {
        source: PathBuf,
        destination: PathBuf,
        bytes: u64,
        replace_existing: bool,
    },
}

/// 已确认提交、明确未提交与回执未知必须分别映射，未知结果不得自动重试。
#[derive(Debug)]
pub(crate) enum LocalCommitError {
    Known(io::Error),
    Unknown(io::Error),
}

impl From<io::Error> for LocalCommitError {
    /// 普通文件系统预检失败证明目标提交尚未开始，因此保留为确定错误。
    fn from(error: io::Error) -> Self {
        Self::Known(error)
    }
}

/// 预先展开本机目录 merge 操作，供异步队列在每个原子项前后可靠申请与记录提交回执。
pub(crate) fn plan_local_archive(
    staging_root: &Path,
    archive_name: &str,
    destination: &Path,
    kind: TransferKind,
    conflict_policy: ConflictPolicy,
) -> Result<Vec<LocalCommitAction>, LocalCommitError> {
    let staged_object = staging_root.join(archive_name);
    let staged_metadata = fs::symlink_metadata(&staged_object)?;
    if staged_metadata.file_type().is_symlink() {
        return Err(invalid_data("暂存对象不能是符号链接").into());
    }
    let mut actions = Vec::new();
    match (kind, staged_metadata.is_file(), staged_metadata.is_dir()) {
        (TransferKind::File, true, _) => {
            plan_file(&staged_object, destination, conflict_policy, &mut actions)?;
        }
        (TransferKind::Directory, _, true) => {
            plan_directory(&staged_object, destination, conflict_policy, &mut actions)?;
        }
        _ => return Err(invalid_data("本机暂存对象类型与传输请求不匹配").into()),
    }
    Ok(actions)
}

/// 执行一项本机提交；文件通过 storage 的同目录原子 rename，目录只创建单个缺失路径。
pub(crate) fn commit_local_action(
    action: &LocalCommitAction,
    owner: &str,
) -> Result<u64, LocalCommitError> {
    match action {
        LocalCommitAction::CreateDirectory { destination } => match fs::create_dir(destination) {
            Ok(()) => Ok(0),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                match fs::symlink_metadata(destination) {
                    Ok(metadata)
                        if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() =>
                    {
                        Ok(0)
                    }
                    Ok(_) => Err(conflict_error("目录目标在提交期间变为非目录项")),
                    Err(error) => Err(LocalCommitError::Known(error)),
                }
            }
            Err(error) => Err(LocalCommitError::Known(error)),
        },
        LocalCommitAction::ReplaceFile {
            source,
            destination,
            bytes,
            replace_existing,
        } => {
            commit_file(source, destination, *replace_existing, owner)?;
            Ok(*bytes)
        }
    }
}

/// 保留目标独有项目，Overwrite 只替换同名普通文件，不把目录误做成整树替换。
fn plan_directory(
    source: &Path,
    destination: &Path,
    conflict_policy: ConflictPolicy,
    actions: &mut Vec<LocalCommitAction>,
) -> Result<(), LocalCommitError> {
    let source_metadata = fs::symlink_metadata(source)?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_dir() {
        return Err(invalid_data("暂存目录类型无效").into());
    }
    let merge_existing = match fs::symlink_metadata(destination) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            actions.push(LocalCommitAction::CreateDirectory {
                destination: destination.to_path_buf(),
            });
            false
        }
        Err(error) => return Err(LocalCommitError::Known(error)),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(conflict_error("目录目标类型与传输源不匹配"));
        }
        Ok(_) if conflict_policy != ConflictPolicy::Overwrite => {
            return Err(conflict_error("目录目标在提交前已出现冲突"));
        }
        Ok(_) => true,
    };

    let mut entries = fs::read_dir(source)?
        .map(|entry| entry.map(|entry| (entry.file_name(), entry.path())))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    for (name, source_entry) in entries {
        let destination_entry = destination.join(name);
        let metadata = fs::symlink_metadata(&source_entry)?;
        if metadata.file_type().is_symlink() {
            return Err(invalid_data("暂存目录包含链接项").into());
        }
        let child_policy = if merge_existing {
            ConflictPolicy::Overwrite
        } else {
            conflict_policy
        };
        if metadata.is_dir() {
            plan_directory(&source_entry, &destination_entry, child_policy, actions)?;
        } else if metadata.is_file() {
            plan_file(&source_entry, &destination_entry, child_policy, actions)?;
        } else {
            return Err(invalid_data("暂存目录包含特殊文件").into());
        }
    }
    Ok(())
}

/// 新文件使用 no-clobber；已有同名普通文件仅在显式 Overwrite 下做原子替换。
fn plan_file(
    source: &Path,
    destination: &Path,
    conflict_policy: ConflictPolicy,
    actions: &mut Vec<LocalCommitAction>,
) -> Result<(), LocalCommitError> {
    let source_metadata = fs::symlink_metadata(source)?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_file() {
        return Err(invalid_data("暂存对象不是普通文件").into());
    }
    let replace_existing = match fs::symlink_metadata(destination) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(LocalCommitError::Known(error)),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(conflict_error("文件目标类型与传输源不匹配"));
        }
        Ok(_) if conflict_policy == ConflictPolicy::Overwrite => true,
        Ok(_) => return Err(conflict_error("文件目标在提交前已出现冲突")),
    };
    actions.push(LocalCommitAction::ReplaceFile {
        source: source.to_path_buf(),
        destination: destination.to_path_buf(),
        bytes: source_metadata.len(),
        replace_existing,
    });
    Ok(())
}

/// 归档解包出的普通 staging 文件先同步内容，再通过同目录 rename 提交以保证耐久性与原子性。
fn commit_file(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
    owner: &str,
) -> Result<(), LocalCommitError> {
    if replace_existing {
        let staging_sibling = replacement_sibling(destination, owner)?;
        commit_staged_file(source, &staging_sibling, CommitMode::MoveStagedNoReplace)
            .map_err(map_atomic_error)?;
        if let Err(error) =
            commit_staged_file(&staging_sibling, destination, CommitMode::ReplaceExisting)
        {
            return match error {
                AtomicCommitError::NotCommitted(error) => match fs::remove_file(&staging_sibling) {
                    Ok(()) => Err(LocalCommitError::Known(error)),
                    Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => {
                        Err(LocalCommitError::Known(error))
                    }
                    Err(cleanup_error) => {
                        tracing::warn!(
                            commit_error_kind = ?error.kind(),
                            cleanup_error_kind = ?cleanup_error.kind(),
                            "本机原子替换未提交且 staging 清理失败"
                        );
                        let kind = error.kind();
                        Err(LocalCommitError::Known(io::Error::new(
                            kind,
                            LocalCommitCleanupFailure {
                                commit_error: error,
                                cleanup_error,
                            },
                        )))
                    }
                },
                AtomicCommitError::CommitUnknown(error) => Err(LocalCommitError::Unknown(error)),
            };
        }
        Ok(())
    } else {
        commit_staged_file(source, destination, CommitMode::MoveStagedNoReplace)
            .map_err(map_atomic_error)
    }
}

/// 同时保留确定未提交的原子操作错误与临时替换文件清理错误。
#[derive(Debug)]
struct LocalCommitCleanupFailure {
    commit_error: io::Error,
    cleanup_error: io::Error,
}

impl Display for LocalCommitCleanupFailure {
    /// 显示两侧 I/O 类别以便诊断，具体路径仍只留在内部 source 中。
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "atomic replacement was not committed ({:?}) and staging cleanup failed ({:?})",
            self.commit_error.kind(),
            self.cleanup_error.kind()
        )
    }
}

impl Error for LocalCommitCleanupFailure {
    /// 错误链优先指向原子提交原因，清理错误也作为聚合值保留供内部审计。
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.commit_error)
    }
}

/// 临时替换文件放在目标同目录，防止 rename 跨卷退化为非原子复制。
fn replacement_sibling(destination: &Path, owner: &str) -> Result<PathBuf, LocalCommitError> {
    let name = destination
        .file_name()
        .ok_or_else(|| LocalCommitError::Known(invalid_data("本机目标缺少文件名")))?;
    let parent = destination
        .parent()
        .ok_or_else(|| LocalCommitError::Known(invalid_data("本机目标缺少父目录")))?;
    Ok(parent.join(format!(
        ".{}.kerminal-{owner}.stage",
        name.to_string_lossy()
    )))
}

/// 保留 storage 对 rename 后同步失败的未知回执分类。
fn map_atomic_error(error: AtomicCommitError) -> LocalCommitError {
    match error {
        AtomicCommitError::NotCommitted(error) => LocalCommitError::Known(error),
        AtomicCommitError::CommitUnknown(error) => LocalCommitError::Unknown(error),
    }
}

/// 用稳定冲突类别表达类型不匹配，不将绝对路径暴露给 UI 或 MCP。
fn conflict_error(message: &'static str) -> LocalCommitError {
    LocalCommitError::Known(io::Error::new(io::ErrorKind::AlreadyExists, message))
}

/// 固定错误文本用于无效归档结构，具体路径仅保留在内部 IO source。
fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
