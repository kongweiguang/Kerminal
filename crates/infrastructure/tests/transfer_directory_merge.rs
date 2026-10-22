//! 目录传输逐项 merge 语义回归测试。
//!
//! @author kongweiguang

use kerminal_domain::files::FileEntryKind;

#[path = "../src/files/transfer/merge.rs"]
mod merge;

use merge::{DirectoryMergeAction, DirectoryMergeError, decide_directory_merge};

/// 新目录和新文件分别进入创建流程。
#[test]
fn absent_destination_creates_source_entry() {
    assert_eq!(
        decide_directory_merge(FileEntryKind::Directory, None),
        Ok(DirectoryMergeAction::CreateDirectory),
    );
    assert_eq!(
        decide_directory_merge(FileEntryKind::File, None),
        Ok(DirectoryMergeAction::CreateFile),
    );
}

/// 同名目录递归合并，同名文件独立原子替换。
#[test]
fn matching_destinations_merge_by_item_type() {
    assert_eq!(
        decide_directory_merge(FileEntryKind::Directory, Some(FileEntryKind::Directory),),
        Ok(DirectoryMergeAction::MergeDirectory),
    );
    assert_eq!(
        decide_directory_merge(FileEntryKind::File, Some(FileEntryKind::File)),
        Ok(DirectoryMergeAction::ReplaceFile),
    );
}

/// 目录合并动作没有 delete/replace-tree 变体，故目标独有项保留。
#[test]
fn merge_contract_never_plans_deleting_destination_only_entries() {
    let actions = [
        decide_directory_merge(FileEntryKind::Directory, None),
        decide_directory_merge(FileEntryKind::Directory, Some(FileEntryKind::Directory)),
        decide_directory_merge(FileEntryKind::File, None),
        decide_directory_merge(FileEntryKind::File, Some(FileEntryKind::File)),
    ];

    assert!(actions.into_iter().all(|action| action.is_ok()));
}

/// 文件与目录类型冲突显式失败，不以覆盖语义替换对方的对象类型。
#[test]
fn mismatched_destination_type_is_rejected() {
    assert_eq!(
        decide_directory_merge(FileEntryKind::Directory, Some(FileEntryKind::File)),
        Err(DirectoryMergeError::TargetTypeMismatch),
    );
    assert_eq!(
        decide_directory_merge(FileEntryKind::File, Some(FileEntryKind::Directory)),
        Err(DirectoryMergeError::TargetTypeMismatch),
    );
}

/// 符号链接和特殊文件不被跟随或解包，防止逃出用户选择的目录树。
#[test]
fn unsupported_source_kinds_are_rejected() {
    assert_eq!(
        decide_directory_merge(FileEntryKind::Symlink, None),
        Err(DirectoryMergeError::UnsupportedSourceType),
    );
    assert_eq!(
        decide_directory_merge(FileEntryKind::Other, None),
        Err(DirectoryMergeError::UnsupportedSourceType),
    );
}
