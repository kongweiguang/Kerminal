//! 文件拖放复制与平台外部路径导入的纯规则测试。
//!
//! @author kongweiguang

use std::{path::PathBuf, sync::Arc};

use kerminal_domain::{
    files::{ConflictPolicy, FileEndpoint, FileEntry, FileEntryKind, TransferKind},
    identity::HostId,
};

use super::{
    DropDirectorySnapshot, DropRejection, FileTransferDrag, copy_target_rejection,
    drop_directory_is_current, drop_targets_are_current, external_drop_directory_ready,
    external_paths_drop_is_allowed, internal_drop_is_allowed, plan_copy_entries,
    plan_internal_drop, validate_drop_entry,
};
use crate::features::files::action_test_support as support;

/// 构造绑定稳定主机身份的 POSIX endpoint，覆盖路径语义不依赖测试宿主系统。
///
/// @author kongweiguang
fn remote(host_id: &HostId, path: &str) -> FileEndpoint {
    FileEndpoint::remote(host_id.clone(), path).expect("valid remote fixture path")
}

/// 构造最小文件行快照，使测试集中验证拖放边界而非列表投影细节。
///
/// @author kongweiguang
fn entry(endpoint: FileEndpoint, name: &str, kind: FileEntryKind) -> FileEntry {
    FileEntry {
        endpoint,
        name: name.to_owned(),
        kind,
        size: None,
        modified: None,
        permissions: None,
        is_hidden: false,
        raw: None,
    }
}

/// 显式绑定目录端点与列表端点的共同生命周期，避免测试闭包无法表达返回借用关系。
///
/// @author kongweiguang
fn directory_snapshot<'a>(
    endpoint: &'a FileEndpoint,
    generation: u64,
    listing_directory: &'a FileEndpoint,
    loading: bool,
) -> DropDirectorySnapshot<'a> {
    DropDirectorySnapshot {
        endpoint,
        generation,
        loading,
        listing_directory: Some(listing_directory),
    }
}

/// 拖动到来源目录本身时应被拒绝，避免生成覆盖自身的无效队列任务。
///
/// @author kongweiguang
#[test]
fn rejects_same_parent_copy() {
    let host = HostId::new();
    let source_root = remote(&host, "/source");
    let source = entry(
        remote(&host, "/source/report.txt"),
        "report.txt",
        FileEntryKind::File,
    );

    assert_eq!(
        validate_drop_entry(&source, &source_root, &source_root),
        Err(DropRejection::SameParent)
    );
}

/// 目录不能复制到自身或它的后代，避免队列持续读取并写入同一棵树。
///
/// @author kongweiguang
#[test]
fn rejects_directory_drop_into_own_descendant() {
    let host = HostId::new();
    let source_root = remote(&host, "/workspace");
    let source = entry(
        remote(&host, "/workspace/archive"),
        "archive",
        FileEntryKind::Directory,
    );
    let destination = remote(&host, "/workspace/archive/current");

    assert_eq!(
        validate_drop_entry(&source, &source_root, &destination),
        Err(DropRejection::TargetInsideSourceDirectory)
    );
}

/// 远端跨主机复制保留双方 HostId，不能把“不同连接”误判为同目录。
///
/// @author kongweiguang
#[test]
fn accepts_copy_between_distinct_hosts() {
    let source_host = HostId::new();
    let target_host = HostId::new();
    let source_root = remote(&source_host, "/logs");
    let source = entry(
        remote(&source_host, "/logs/app.log"),
        "app.log",
        FileEntryKind::File,
    );
    let destination = remote(&target_host, "/archive");

    assert_eq!(
        validate_drop_entry(&source, &source_root, &destination),
        Ok(("app.log".to_owned(), TransferKind::File))
    );
}

/// 拖放资格必须同时满足跨面板、目录已加载且当前没有任务入队。
///
/// @author kongweiguang
#[test]
fn rejects_same_side_busy_and_unloaded_internal_drop() {
    let source_host = HostId::new();
    let target_host = HostId::new();
    let source_root = remote(&source_host, "/source");
    let source = entry(
        remote(&source_host, "/source/data.bin"),
        "data.bin",
        FileEntryKind::File,
    );
    let drag = FileTransferDrag::new(17, 0, source_root.clone(), 3, Arc::new(vec![source]), 0);
    let target = remote(&target_host, "/target");
    let source_snapshot = DropDirectorySnapshot {
        endpoint: &source_root,
        generation: 3,
        loading: false,
        listing_directory: Some(&source_root),
    };

    assert!(!internal_drop_is_allowed(
        &drag,
        17,
        0,
        source_snapshot,
        0,
        &target,
        true,
        false
    ));
    assert!(!internal_drop_is_allowed(
        &drag,
        17,
        0,
        source_snapshot,
        1,
        &target,
        false,
        false
    ));
    assert!(!internal_drop_is_allowed(
        &drag,
        17,
        0,
        source_snapshot,
        1,
        &target,
        true,
        true
    ));
    assert!(internal_drop_is_allowed(
        &drag,
        17,
        0,
        source_snapshot,
        1,
        &target,
        true,
        false
    ));
    assert!(!internal_drop_is_allowed(
        &drag,
        18,
        0,
        source_snapshot,
        1,
        &target,
        true,
        false
    ));
    assert!(!internal_drop_is_allowed(
        &drag,
        17,
        0,
        DropDirectorySnapshot {
            generation: 4,
            ..source_snapshot
        },
        1,
        &target,
        true,
        false,
    ));
    assert!(!internal_drop_is_allowed(
        &drag,
        17,
        0,
        DropDirectorySnapshot {
            loading: true,
            ..source_snapshot
        },
        1,
        &target,
        true,
        false,
    ));
    assert!(!internal_drop_is_allowed(
        &drag,
        17,
        0,
        DropDirectorySnapshot {
            listing_directory: None,
            ..source_snapshot
        },
        1,
        &target,
        true,
        false,
    ));
}

/// 释放时再次验证 FilesPane 身份，即使其他实体的路径和代次完全相同也不能接收拖动。
///
/// @author kongweiguang
#[gpui_kit::test]
fn release_rejects_a_different_pane_with_identical_directories(app: &mut gpui_kit::TestAppContext) {
    let source = remote(&HostId::new(), "/source");
    let target = remote(&HostId::new(), "/target");
    let file = entry(
        super::child_endpoint(&source, "file.txt").expect("valid child"),
        "file.txt",
        FileEntryKind::File,
    );
    let seeded_file = file.clone();
    let seeded_target = target.clone();
    let fixture = support::entity_fixture(app, source.clone(), move |pane| {
        pane.panels[0].endpoint = seeded_target;
        support::seed_listing(pane, 0, 4, Vec::new());
        support::seed_listing(pane, 1, 3, vec![seeded_file]);
    });
    let _runtime = fixture.runtime;
    let pane_id = fixture.pane.entity_id().as_u64();
    let drag = FileTransferDrag::new(pane_id, 1, source, 3, Arc::new(vec![file]), 0);
    app.update(|cx| {
        fixture.pane.update(cx, |pane, _| {
            assert!(drop_targets_are_current(
                pane, pane_id, &drag, 0, &target, 4
            ));
            assert!(!drop_targets_are_current(
                pane,
                pane_id.wrapping_add(1),
                &drag,
                0,
                &target,
                4,
            ));
        });
    });
}

/// 符号链接保持不可拖传，避免一次看似普通的拖动触发链接跟随。
///
/// @author kongweiguang
#[test]
fn rejects_symbolic_link_transfer() {
    let host = HostId::new();
    let source_root = remote(&host, "/source");
    let link = entry(
        remote(&host, "/source/shortcut"),
        "shortcut",
        FileEntryKind::Symlink,
    );
    let destination = remote(&HostId::new(), "/target");

    assert_eq!(
        validate_drop_entry(&link, &source_root, &destination),
        Err(DropRejection::UnsupportedKind)
    );
}

/// 外部系统路径只能进入已加载的远端或容器目录，不能把普通本机目录拖成队列任务。
///
/// @author kongweiguang
#[test]
fn allows_external_paths_only_for_ready_remote_targets() {
    let host = HostId::new();
    let remote_target = remote(&host, "/uploads");
    let local_target = FileEndpoint::Local {
        path: PathBuf::from("uploads"),
    };

    assert!(external_drop_directory_ready(
        &remote_target,
        Some(&remote_target),
        false
    ));
    assert!(!external_drop_directory_ready(
        &remote_target,
        Some(&remote_target),
        true
    ));
    assert!(external_paths_drop_is_allowed(
        &remote_target,
        true,
        false,
        true
    ));
    assert!(!external_paths_drop_is_allowed(
        &remote_target,
        false,
        false,
        true
    ));
    assert!(!external_paths_drop_is_allowed(
        &remote_target,
        true,
        true,
        true
    ));
    assert!(!external_paths_drop_is_allowed(
        &local_target,
        true,
        false,
        true
    ));
}

/// 本机文件可在两个不同目录之间拖放，随后由既有 TransferService 执行复制。
///
/// @author kongweiguang
#[test]
fn accepts_local_copy_between_distinct_directories() {
    let source_path = PathBuf::from("source");
    let source_root = FileEndpoint::Local {
        path: source_path.clone(),
    };
    let source = entry(
        FileEndpoint::Local {
            path: source_path.join("note.txt"),
        },
        "note.txt",
        FileEntryKind::File,
    );
    let target_root = FileEndpoint::Local {
        path: PathBuf::from("target"),
    };

    assert_eq!(
        validate_drop_entry(&source, &source_root, &target_root),
        Ok(("note.txt".to_owned(), TransferKind::File))
    );
}

/// 同目录拖放没有有效复制结果，避免向队列提交必然冲突或空操作。
///
/// @author kongweiguang
#[test]
fn rejects_local_copy_into_same_parent() {
    let source_root = FileEndpoint::Local {
        path: PathBuf::from("workspace"),
    };
    let source = entry(
        FileEndpoint::Local {
            path: PathBuf::from("workspace/note.txt"),
        },
        "note.txt",
        FileEntryKind::File,
    );

    assert_eq!(
        validate_drop_entry(&source, &source_root, &source_root),
        Err(DropRejection::SameParent)
    );
}

/// 本机目录不能拖入自身后代，避免递归把新生成的数据当成源继续复制。
///
/// @author kongweiguang
#[test]
fn rejects_local_directory_copy_into_own_descendant() {
    let source_root = FileEndpoint::Local {
        path: PathBuf::from("workspace"),
    };
    let source = entry(
        FileEndpoint::Local {
            path: PathBuf::from("workspace/archive"),
        },
        "archive",
        FileEntryKind::Directory,
    );
    let destination = FileEndpoint::Local {
        path: PathBuf::from("workspace/archive/nested"),
    };

    assert_eq!(
        validate_drop_entry(&source, &source_root, &destination),
        Err(DropRejection::TargetInsideSourceDirectory)
    );
}

/// 释放时来源或目标目录已换代、正在加载或列表仍指向旧路径，都必须要求重新拖放。
///
/// @author kongweiguang
#[test]
fn rejects_stale_source_and_target_directory_snapshots() {
    let source_host = HostId::new();
    let target_host = HostId::new();
    let source = remote(&source_host, "/source");
    let target = remote(&target_host, "/target");

    assert!(drop_directory_is_current(
        &source,
        7,
        directory_snapshot(&source, 7, &source, false)
    ));
    assert!(!drop_directory_is_current(
        &source,
        7,
        directory_snapshot(&source, 8, &source, false)
    ));
    assert!(!drop_directory_is_current(
        &target,
        3,
        directory_snapshot(&target, 4, &target, false)
    ));
    assert!(!drop_directory_is_current(
        &target,
        3,
        directory_snapshot(&target, 3, &source, false)
    ));
    assert!(!drop_directory_is_current(
        &source,
        7,
        directory_snapshot(&target, 7, &target, false)
    ));
    assert!(!drop_directory_is_current(
        &target,
        3,
        directory_snapshot(&target, 3, &target, true)
    ));
}

/// 拖动保留按下时的多选快照；之后切换选择或浏览目标不会改写已发出的载荷。
///
/// @author kongweiguang
#[test]
fn keeps_selected_entries_frozen_while_panel_selection_changes() {
    let host = HostId::new();
    let source_root = remote(&host, "/source");
    let first = entry(
        remote(&host, "/source/first.txt"),
        "first.txt",
        FileEntryKind::File,
    );
    let second = entry(
        remote(&host, "/source/second.txt"),
        "second.txt",
        FileEntryKind::File,
    );
    let drag = FileTransferDrag::new(
        17,
        0,
        source_root,
        12,
        Arc::new(vec![first.clone(), second.clone()]),
        0,
    );
    let later_selection = Arc::new(vec![entry(
        remote(&host, "/source/later.txt"),
        "later.txt",
        FileEntryKind::File,
    )]);

    assert_eq!(drag.entries(), &[first, second]);
    assert_eq!(later_selection[0].name, "later.txt");
    assert_eq!(drag.selected_count(), 2);
}

/// 多选拖放遇到失败项目时仍提交有效项，同时把拒绝原因留给用户；全失败时不伪造任务。
///
/// @author kongweiguang
#[test]
fn mixed_selection_queues_valid_items_and_preserves_failure_feedback() {
    let source_host = HostId::new();
    let target_host = HostId::new();
    let source_root = remote(&source_host, "/source");
    let valid = entry(
        remote(&source_host, "/source/report.txt"),
        "report.txt",
        FileEntryKind::File,
    );
    let rejected = entry(
        remote(&source_host, "/source/shortcut"),
        "shortcut",
        FileEntryKind::Symlink,
    );
    let drag = FileTransferDrag::new(17, 0, source_root, 1, Arc::new(vec![valid, rejected]), 0);
    let target = remote(&target_host, "/archive");

    let plan = plan_internal_drop(&drag, &target, ConflictPolicy::Rename);
    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.items[0].0, "report.txt");
    assert_eq!(plan.items[0].1.conflict_policy, ConflictPolicy::Rename);
    assert_eq!(plan.issues.len(), 1);
    assert!(plan.issues[0].contains("shortcut"));

    let rejected_only = FileTransferDrag::new(
        17,
        0,
        remote(&source_host, "/source"),
        1,
        Arc::new(vec![entry(
            remote(&source_host, "/source/shortcut"),
            "shortcut",
            FileEntryKind::Symlink,
        )]),
        0,
    );
    let failed_plan = plan_internal_drop(&rejected_only, &target, ConflictPolicy::Rename);
    assert!(failed_plan.items.is_empty());
    assert_eq!(failed_plan.issues.len(), 1);
}

/// 菜单与工具栏使用同一计划：无效目标拒绝入队，混合选区保留可复制项及拒绝原因。
///
/// @author kongweiguang
#[test]
fn copy_plan_rejects_same_directory_and_keeps_safe_mixed_selection() {
    let host = HostId::new();
    let root = remote(&host, "/source");
    let folder = entry(
        remote(&host, "/source/folder"),
        "folder",
        FileEntryKind::Directory,
    );
    let file = entry(
        remote(&host, "/source/report.txt"),
        "report.txt",
        FileEntryKind::File,
    );
    let items = vec![folder.clone(), file.clone()];

    assert_eq!(
        copy_target_rejection(items.iter(), &root, &root),
        Some(DropRejection::SameParent)
    );
    let (same_items, same_issues) = plan_copy_entries(&items, &root, &root, ConflictPolicy::Rename);
    assert!(same_items.is_empty());
    assert_eq!(same_issues.len(), 2);

    let target = remote(&host, "/source/folder/child");
    assert_eq!(copy_target_rejection(items.iter(), &root, &target), None);
    let (valid, issues) = plan_copy_entries(&items, &root, &target, ConflictPolicy::Rename);
    assert_eq!(valid.len(), 1);
    assert_eq!(valid[0].0, "report.txt");
    assert_eq!(valid[0].1.source, file.endpoint);
    assert_eq!(issues.len(), 1);
    assert!(issues[0].contains("folder"));
}

/// 树选中的父目录不可复制时，内部拖放仍提交安全子项；外部拖出保留原折叠根项。
///
/// @author kongweiguang
#[test]
fn tree_drag_preserves_safe_child_when_selected_parent_is_rejected() {
    let host = HostId::new();
    let root = remote(&host, "/source");
    let folder = entry(
        remote(&host, "/source/folder"),
        "folder",
        FileEntryKind::Directory,
    );
    let file = entry(
        remote(&host, "/source/folder/file.txt"),
        "file.txt",
        FileEntryKind::File,
    );
    let drag = FileTransferDrag::new(17, 0, root.clone(), 1, Arc::new(vec![folder.clone()]), 1)
        .with_tree_selection(Arc::new(vec![file.clone(), folder.clone()]));
    let nested_target = remote(&host, "/source/folder/target");
    let plan = plan_internal_drop(&drag, &nested_target, ConflictPolicy::Rename);
    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.items[0].1.source, file.endpoint);
    assert_eq!(plan.issues.len(), 1);
    assert_eq!(plan.included_descendants, 0);
    assert_eq!(drag.entries(), &[folder]);
    assert_eq!(drag.selected_count(), 2);

    let other_target = remote(&HostId::new(), "/target");
    let covered_plan = plan_internal_drop(&drag, &other_target, ConflictPolicy::Rename);
    assert_eq!(covered_plan.items.len(), 1);
    assert_eq!(covered_plan.included_descendants, 1);
    assert!(covered_plan.issues.is_empty());
}
