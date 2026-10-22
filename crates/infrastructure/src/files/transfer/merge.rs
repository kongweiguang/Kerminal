//! 目录复制逐项合并决策；只替换同名文件，不删除目标目录独有内容。
//!
//! @author kongweiguang

use kerminal_domain::files::FileEntryKind;

/// 单个源项与目标项匹配后的最小提交动作，不含删除或整树替换选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryMergeAction {
    CreateDirectory,
    MergeDirectory,
    CreateFile,
    ReplaceFile,
}

/// 文件类型冲突和不支持的源类型必须停止该项提交并保留已确认的其他结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryMergeError {
    TargetTypeMismatch,
    UnsupportedSourceType,
}

/// 为一个源项选择目录合并动作；目标独有项不进入决策集合，因此不会被静默删除。
pub fn decide_directory_merge(
    source: FileEntryKind,
    destination: Option<FileEntryKind>,
) -> Result<DirectoryMergeAction, DirectoryMergeError> {
    match (source, destination) {
        (FileEntryKind::Directory, None) => Ok(DirectoryMergeAction::CreateDirectory),
        (FileEntryKind::Directory, Some(FileEntryKind::Directory)) => {
            Ok(DirectoryMergeAction::MergeDirectory)
        }
        (FileEntryKind::Directory, Some(_)) => Err(DirectoryMergeError::TargetTypeMismatch),
        (FileEntryKind::File, None) => Ok(DirectoryMergeAction::CreateFile),
        (FileEntryKind::File, Some(FileEntryKind::File)) => Ok(DirectoryMergeAction::ReplaceFile),
        (FileEntryKind::File, Some(_)) => Err(DirectoryMergeError::TargetTypeMismatch),
        (FileEntryKind::Symlink | FileEntryKind::Other, _) => {
            Err(DirectoryMergeError::UnsupportedSourceType)
        }
    }
}
