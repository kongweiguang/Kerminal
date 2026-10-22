//! 文件树的惰性加载、端点隔离与可见选择回归。
//!
//! @author kongweiguang

use super::*;
use kerminal_domain::{
    files::{ContainerRuntime, RemotePath},
    identity::HostId,
};

/// 为模型测试创建规范远端端点，避免测试依赖本机路径分隔符。
///
/// @author kongweiguang
fn remote_endpoint(host: &HostId, path: &str) -> FileEndpoint {
    FileEndpoint::Remote {
        host_id: host.clone(),
        path: RemotePath::parse(path).expect("valid test remote path"),
    }
}

/// 用真实父 endpoint 构造目录项，固定树请求的父子路径校验边界。
///
/// @author kongweiguang
fn remote_entry(parent: &FileEndpoint, name: &str, kind: FileEntryKind) -> FileEntry {
    let FileEndpoint::Remote { host_id, path } = parent else {
        panic!("test fixture must use a remote endpoint");
    };
    FileEntry {
        endpoint: FileEndpoint::Remote {
            host_id: host_id.clone(),
            path: path.join_name(name).expect("valid child name"),
        },
        name: name.to_owned(),
        kind,
        size: None,
        modified: None,
        permissions: None,
        is_hidden: name.starts_with('.'),
        raw: None,
    }
}

/// 安装一个已完成的根列表，使后续测试只关注树模型的增量行为。
///
/// @author kongweiguang
fn install_root(
    tree: &mut FileTreeBrowser,
    root: FileEndpoint,
    entries: Vec<FileEntry>,
    generation: u64,
) {
    tree.begin_root_request(&root, generation);
    tree.set_root_listing(root, generation, entries, true);
}

/// 完成一次目录读取，使用端点校验过的 listing 驱动懒加载缓存。
///
/// @author kongweiguang
fn complete_directory(
    tree: &mut FileTreeBrowser,
    directory: &FileEndpoint,
    generation: u64,
    entries: Vec<FileEntry>,
) {
    let (request_id, _) = tree
        .begin_directory_request(directory, generation)
        .expect("directory request should be admitted");
    assert!(tree.complete_directory_request(
        directory,
        generation,
        request_id,
        true,
        Ok(DirectoryListing {
            directory: directory.clone(),
            entries,
        }),
    ));
}

/// 折叠父目录会取消正在读取的后代、清除不可见选择并拒绝迟到结果。
///
/// @author kongweiguang
#[test]
fn collapsing_directory_cancels_descendants_and_drops_hidden_selection() {
    let host = HostId::new();
    let root = remote_endpoint(&host, "/workspace");
    let folder = remote_endpoint(&host, "/workspace/folder");
    let nested = remote_endpoint(&host, "/workspace/folder/nested");
    let mut tree = FileTreeBrowser::default();
    install_root(
        &mut tree,
        root.clone(),
        vec![remote_entry(&root, "folder", FileEntryKind::Directory)],
        1,
    );

    tree.expand(&folder);
    complete_directory(
        &mut tree,
        &folder,
        1,
        vec![remote_entry(&folder, "nested", FileEntryKind::Directory)],
    );
    tree.expand(&nested);
    let (request_id, cancellation) = tree
        .begin_directory_request(&nested, 1)
        .expect("nested directory starts loading");
    let visible = tree.visible_endpoints(true);
    tree.apply_selection(&nested, &visible, SelectionGesture::Replace);
    assert!(tree.selected.contains(&nested));

    assert_eq!(tree.collapse(&folder), 1);
    assert!(cancellation.cancelled.load(Ordering::Acquire));
    assert!(!tree.expanded.contains(&nested));
    assert!(!tree.complete_directory_request(
        &nested,
        1,
        request_id,
        true,
        Ok(DirectoryListing {
            directory: nested.clone(),
            entries: vec![remote_entry(&nested, "late.txt", FileEntryKind::File)],
        }),
    ));
    assert!(tree.selected_entries(true).is_empty());

    tree.cancel_pending_requests();
}

/// 切换完整根 endpoint 会取消旧请求、清空旧端点选择并隔离其迟到响应。
///
/// @author kongweiguang
#[test]
fn changing_endpoint_rejects_old_response_and_selection() {
    let host = HostId::new();
    let old_root = remote_endpoint(&host, "/old");
    let old_folder = remote_endpoint(&host, "/old/folder");
    let new_root = remote_endpoint(&host, "/new");
    let mut tree = FileTreeBrowser::default();
    install_root(
        &mut tree,
        old_root.clone(),
        vec![remote_entry(&old_root, "folder", FileEntryKind::Directory)],
        1,
    );
    let visible = tree.visible_endpoints(true);
    tree.apply_selection(&old_folder, &visible, SelectionGesture::Replace);
    tree.expand(&old_folder);
    let (request_id, cancellation) = tree
        .begin_directory_request(&old_folder, 1)
        .expect("old endpoint request starts");

    tree.begin_root_request(&new_root, 2);
    assert!(cancellation.cancelled.load(Ordering::Acquire));
    assert!(!tree.complete_directory_request(
        &old_folder,
        1,
        request_id,
        true,
        Ok(DirectoryListing {
            directory: old_folder.clone(),
            entries: vec![remote_entry(&old_folder, "stale.txt", FileEntryKind::File)],
        }),
    ));
    tree.set_root_listing(
        new_root.clone(),
        2,
        vec![remote_entry(&new_root, "current.txt", FileEntryKind::File)],
        true,
    );

    assert!(tree.selected_entries(true).is_empty());
    assert_eq!(
        tree.root_entries(true)[0].endpoint,
        remote_endpoint(&host, "/new/current.txt")
    );
}

/// 目录错误保留绑定原 endpoint 的重试行，重试创建新请求而不复用旧回执。
///
/// @author kongweiguang
#[test]
fn failed_directory_has_explicit_retry_node() {
    let host = HostId::new();
    let root = remote_endpoint(&host, "/workspace");
    let folder = remote_endpoint(&host, "/workspace/folder");
    let mut tree = FileTreeBrowser::default();
    install_root(
        &mut tree,
        root.clone(),
        vec![remote_entry(&root, "folder", FileEntryKind::Directory)],
        7,
    );
    tree.expand(&folder);
    let (first_request, _) = tree
        .begin_directory_request(&folder, 7)
        .expect("first request starts");
    assert!(tree.complete_directory_request(
        &folder,
        7,
        first_request,
        true,
        Err("connection interrupted".to_owned()),
    ));

    let projection = tree.projection(true);
    assert!(
        projection
            .nodes
            .nodes
            .values()
            .any(|node| node == &TreeNode::Retry(folder.clone()))
    );

    let (retry_request, cancellation) = tree
        .begin_directory_request(&folder, 7)
        .expect("failed directory can be retried");
    assert_ne!(first_request, retry_request);
    assert!(tree.complete_directory_request(
        &folder,
        7,
        retry_request,
        true,
        Ok(DirectoryListing {
            directory: folder.clone(),
            entries: vec![remote_entry(&folder, "ready.txt", FileEntryKind::File)],
        }),
    ));
    assert!(
        tree.visible_endpoints(true)
            .contains(&remote_endpoint(&host, "/workspace/folder/ready.txt"))
    );
    assert!(!cancellation.cancelled.load(Ordering::Acquire));
}

/// 同名文件保留各自的完整远端 endpoint，批量选择不会退化成 basename 匹配。
///
/// @author kongweiguang
#[test]
fn same_name_entries_remain_distinct_by_full_endpoint() {
    let host = HostId::new();
    let root = remote_endpoint(&host, "/workspace");
    let alpha = remote_endpoint(&host, "/workspace/alpha");
    let beta = remote_endpoint(&host, "/workspace/beta");
    let alpha_file = remote_endpoint(&host, "/workspace/alpha/shared.txt");
    let beta_file = remote_endpoint(&host, "/workspace/beta/shared.txt");
    let mut tree = FileTreeBrowser::default();
    install_root(
        &mut tree,
        root.clone(),
        vec![
            remote_entry(&root, "alpha", FileEntryKind::Directory),
            remote_entry(&root, "beta", FileEntryKind::Directory),
        ],
        3,
    );
    tree.expand(&alpha);
    complete_directory(
        &mut tree,
        &alpha,
        3,
        vec![remote_entry(&alpha, "shared.txt", FileEntryKind::File)],
    );
    tree.expand(&beta);
    complete_directory(
        &mut tree,
        &beta,
        3,
        vec![remote_entry(&beta, "shared.txt", FileEntryKind::File)],
    );
    tree.select_endpoints([alpha_file.clone(), beta_file.clone()]);

    assert_eq!(
        tree.selected_entries(true)
            .into_iter()
            .map(|entry| entry.endpoint)
            .collect::<Vec<_>>(),
        vec![alpha_file, beta_file]
    );
}

/// 已选目录只吸收同一完整 endpoint 下的后代，不误合并相似路径或另一目标的同名项。
///
/// @author kongweiguang
#[test]
fn selected_directory_coverage_respects_full_endpoint_identity() {
    let host = HostId::new();
    let other_host = HostId::new();
    let remote_directory = remote_endpoint(&host, "/workspace/folder");
    let remote_child = remote_endpoint(&host, "/workspace/folder/shared.txt");
    let prefix_lookalike = remote_endpoint(&host, "/workspace/folder2/shared.txt");
    let other_host_child = remote_endpoint(&other_host, "/workspace/folder/shared.txt");
    let remote_sibling = remote_endpoint(&host, "/workspace/other/shared.txt");
    let container_directory = FileEndpoint::Container {
        host_id: Some(host.clone()),
        runtime: ContainerRuntime::Podman,
        container_id: "container-a".to_owned(),
        path: RemotePath::parse("/workspace/folder").expect("valid container path"),
    };
    let container_child = FileEndpoint::Container {
        host_id: Some(host.clone()),
        runtime: ContainerRuntime::Podman,
        container_id: "container-a".to_owned(),
        path: RemotePath::parse("/workspace/folder/shared.txt").expect("valid container path"),
    };
    let other_container_child = FileEndpoint::Container {
        host_id: Some(host),
        runtime: ContainerRuntime::Podman,
        container_id: "container-b".to_owned(),
        path: RemotePath::parse("/workspace/folder/shared.txt").expect("valid container path"),
    };
    let selected_directories = [remote_directory, container_directory]
        .into_iter()
        .collect::<HashSet<_>>();

    assert!(has_selected_directory_ancestor(
        &remote_child,
        &selected_directories
    ));
    assert!(has_selected_directory_ancestor(
        &container_child,
        &selected_directories
    ));
    assert!(!has_selected_directory_ancestor(
        &prefix_lookalike,
        &selected_directories
    ));
    assert!(!has_selected_directory_ancestor(
        &other_host_child,
        &selected_directories
    ));
    assert!(!has_selected_directory_ancestor(
        &other_container_child,
        &selected_directories
    ));
    assert!(!has_selected_directory_ancestor(
        &remote_sibling,
        &selected_directories
    ));
}

/// 树传输计划只保留所选目录根项，明确统计由目录任务覆盖的显式子项且不改原选择。
///
/// @author kongweiguang
#[test]
fn tree_transfer_plan_queues_selected_directory_roots_once() {
    let host = HostId::new();
    let root = remote_endpoint(&host, "/workspace");
    let directory = remote_endpoint(&host, "/workspace/folder");
    let nested = remote_endpoint(&host, "/workspace/folder/nested");
    let nested_file = remote_endpoint(&host, "/workspace/folder/nested/inside.txt");
    let folder_file = remote_endpoint(&host, "/workspace/folder/root.txt");
    let loose_file = remote_endpoint(&host, "/workspace/loose.txt");
    let mut tree = FileTreeBrowser::default();
    install_root(
        &mut tree,
        root.clone(),
        vec![
            remote_entry(&root, "folder", FileEntryKind::Directory),
            remote_entry(&root, "loose.txt", FileEntryKind::File),
        ],
        9,
    );
    tree.expand(&directory);
    complete_directory(
        &mut tree,
        &directory,
        9,
        vec![
            remote_entry(&directory, "nested", FileEntryKind::Directory),
            remote_entry(&directory, "root.txt", FileEntryKind::File),
        ],
    );
    tree.expand(&nested);
    complete_directory(
        &mut tree,
        &nested,
        9,
        vec![remote_entry(&nested, "inside.txt", FileEntryKind::File)],
    );
    tree.select_endpoints([
        directory.clone(),
        nested.clone(),
        nested_file,
        folder_file,
        loose_file.clone(),
    ]);

    let plan = tree.transfer_selection_plan(true);

    assert_eq!(
        plan.roots
            .into_iter()
            .map(|entry| entry.endpoint)
            .collect::<Vec<_>>(),
        vec![directory, loose_file]
    );
    assert_eq!(plan.included_descendants, 3);
    assert_eq!(tree.selected_entries(true).len(), 5);
}

/// 删除顺序对可见 DFS preorder 做栈式后序投影，父项晚于已选后代且兄弟顺序稳定。
///
/// @author kongweiguang
#[test]
fn tree_delete_plan_is_stable_postorder_without_sibling_reordering() {
    let host = HostId::new();
    let root = remote_endpoint(&host, "/workspace");
    let first = remote_endpoint(&host, "/workspace/first");
    let nested = remote_endpoint(&host, "/workspace/first/nested");
    let unselected_file = remote_endpoint(&host, "/workspace/first/nested/unselected.txt");
    let second = remote_endpoint(&host, "/workspace/second");
    let mut tree = FileTreeBrowser::default();
    install_root(
        &mut tree,
        root.clone(),
        vec![
            remote_entry(&root, "first", FileEntryKind::Directory),
            remote_entry(&root, "second", FileEntryKind::Directory),
            remote_entry(&root, "z-root.txt", FileEntryKind::File),
        ],
        12,
    );
    tree.expand(&first);
    complete_directory(
        &mut tree,
        &first,
        12,
        vec![
            remote_entry(&first, "nested", FileEntryKind::Directory),
            remote_entry(&first, "z-first.txt", FileEntryKind::File),
        ],
    );
    tree.expand(&nested);
    complete_directory(
        &mut tree,
        &nested,
        12,
        vec![
            remote_entry(&nested, "leaf.txt", FileEntryKind::File),
            remote_entry(&nested, "unselected.txt", FileEntryKind::File),
        ],
    );
    tree.expand(&second);
    complete_directory(
        &mut tree,
        &second,
        12,
        vec![remote_entry(&second, "child.txt", FileEntryKind::File)],
    );
    tree.select_endpoints([
        remote_endpoint(&host, "/workspace/first/nested/leaf.txt"),
        nested.clone(),
        remote_endpoint(&host, "/workspace/first/z-first.txt"),
        first.clone(),
        remote_endpoint(&host, "/workspace/second/child.txt"),
        second.clone(),
        remote_endpoint(&host, "/workspace/z-root.txt"),
    ]);

    let deleted = tree
        .selected_entries_postorder(true)
        .into_iter()
        .map(|entry| entry.endpoint)
        .collect::<Vec<_>>();
    assert_eq!(
        deleted,
        vec![
            remote_endpoint(&host, "/workspace/first/nested/leaf.txt"),
            nested,
            remote_endpoint(&host, "/workspace/first/z-first.txt"),
            first,
            remote_endpoint(&host, "/workspace/second/child.txt"),
            second,
            remote_endpoint(&host, "/workspace/z-root.txt"),
        ]
    );
    assert!(!deleted.contains(&unselected_file));
}
