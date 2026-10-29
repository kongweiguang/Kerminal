use super::*;
use crate::features::files::{
    DirectoryPathError, create_directory_destination, file_list_row_height,
};
use crate::features::files::{
    FileSort, FileSortColumn, bookmark_response_is_current, bookmark_target,
    permissions::{PermissionEditor, supports_posix_mode_endpoint},
    transfer_queue::TransferQueueUi,
    view_mode::FileEntryFilter,
    workspace_projection_push_is_newer, workspace_projection_request_is_current,
    workspace_projection_response_is_fresh,
};
use kerminal_application::preferences::InterfaceDensity;
use kerminal_domain::{
    files::{
        BookmarkTarget, ConflictPolicy, ContainerRuntime, FileBookmark, FileEntryKind, RemotePath,
    },
    identity::HostId,
};
use std::path::PathBuf;

/// 文件浏览器总是将路径和操作分成两行，SSH 跟随只属于紧凑远程栏。
///
/// @author kongweiguang
#[test]
fn browser_toolbar_projects_two_rows_and_compact_ssh_follow() {
    let remote = FileEndpoint::remote(HostId::new(), "/srv/app").unwrap();
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let compact_remote = FileBrowserToolbarProjection::for_endpoint(true, &remote);
    let wide_remote = FileBrowserToolbarProjection::for_endpoint(false, &remote);
    let compact_local = FileBrowserToolbarProjection::for_endpoint(true, &local);

    for projection in [compact_remote, wide_remote, compact_local] {
        assert_eq!(
            projection.rows,
            [FileBrowserToolbarRow::Path, FileBrowserToolbarRow::Actions]
        );
    }
    assert!(compact_remote.show_terminal_follow);
    assert!(!wide_remote.show_terminal_follow);
    assert!(!compact_local.show_terminal_follow);
}

/// 表格宽度边界与紧凑工具栏固定图标布局相互独立。
///
/// @author kongweiguang
#[test]
fn browser_columns_respect_width_boundary() {
    assert!(!FileBrowserLayout::for_width(px(399.)).columns.size);
    assert!(FileBrowserLayout::for_width(px(400.)).columns.size);
    assert!(!FileBrowserLayout::for_width(px(539.)).columns.permissions);
    assert!(FileBrowserLayout::for_width(px(540.)).columns.permissions);
}

/// 当前请求加载、失败、空列表和已加载列表必须对应互斥菜单内容。
///
/// @author kongweiguang
#[test]
fn bookmark_menu_projects_loading_error_empty_and_items() {
    let bookmark = FileBookmark {
        created_at_unix_ms: 0,
        target: BookmarkTarget::Remote {
            host_id: HostId::new(),
        },
        path: "/srv/app".to_owned(),
        label: None,
    };
    let bookmarks = vec![bookmark];

    assert_eq!(
        bookmark_menu_state(true, Some("stale error"), bookmarks.as_slice()),
        BookmarkMenuState::Loading
    );
    assert_eq!(
        bookmark_menu_state(false, Some("read failed"), &[]),
        BookmarkMenuState::Error("read failed")
    );
    assert_eq!(
        bookmark_menu_state(false, None, &[]),
        BookmarkMenuState::Empty
    );
    assert_eq!(
        bookmark_menu_state(false, None, bookmarks.as_slice()),
        BookmarkMenuState::Items(bookmarks.as_slice())
    );
}

/// 新建目录对话框接受相对或绝对路径，并拒绝远端根路径及无法解析的输入。
///
/// @author kongweiguang
#[test]
fn directory_creation_resolves_full_paths_and_rejects_roots() {
    let host = HostId::new();
    let current = FileEndpoint::remote(host.clone(), "/srv/app").unwrap();
    let relative = create_directory_destination(&current, "nested/new-folder").unwrap();
    let absolute = create_directory_destination(&current, "/var/tmp/new-folder").unwrap();

    assert_eq!(
        relative.remote_path().unwrap().as_str(),
        "/srv/app/nested/new-folder"
    );
    assert_eq!(
        absolute.remote_path().unwrap().as_str(),
        "/var/tmp/new-folder"
    );
    assert_eq!(
        create_directory_destination(&current, "/"),
        Err(DirectoryPathError::Root)
    );
    assert_eq!(
        create_directory_destination(&current, "  "),
        Err(DirectoryPathError::Empty)
    );
    let relative_current = FileEndpoint::Remote {
        host_id: host,
        path: RemotePath::parse(".").unwrap(),
    };
    assert_eq!(
        create_directory_destination(&relative_current, "new-folder"),
        Err(DirectoryPathError::NotAbsolute)
    );
}

/// 文件行高与 main 的 Compact、Comfortable、Spacious 设置映射一致。
///
/// @author kongweiguang
#[test]
fn file_rows_follow_interface_density() {
    assert_eq!(file_list_row_height(InterfaceDensity::Compact), 36.0);
    assert_eq!(file_list_row_height(InterfaceDensity::Comfortable), 44.0);
    assert_eq!(file_list_row_height(InterfaceDensity::Spacious), 48.0);
}

/// 目录请求可填充同版本首个快照，但过期返回不能覆盖已经推送的新状态。
///
/// @author kongweiguang
#[test]
fn editor_projection_request_rejects_older_revision() {
    assert!(workspace_projection_response_is_fresh(None, 0));
    assert!(workspace_projection_response_is_fresh(Some(4), 4));
    assert!(workspace_projection_response_is_fresh(Some(4), 5));
    assert!(!workspace_projection_response_is_fresh(Some(5), 4));
}

/// 快速切出并重进工作区后，旧视图代次的回包不能填充新请求。
///
/// @author kongweiguang
#[test]
fn editor_projection_request_rejects_stale_view_generation() {
    assert!(workspace_projection_request_is_current(true, 7, 7));
    assert!(!workspace_projection_request_is_current(true, 6, 7));
    assert!(!workspace_projection_request_is_current(false, 7, 7));
}

/// 实时推送只接受新代次，重复及迟到快照不能覆盖活动 pane。
///
/// @author kongweiguang
#[test]
fn editor_projection_push_requires_a_newer_revision() {
    assert!(workspace_projection_push_is_newer(None, 0));
    assert!(workspace_projection_push_is_newer(Some(4), 5));
    assert!(!workspace_projection_push_is_newer(Some(4), 4));
    assert!(!workspace_projection_push_is_newer(Some(5), 4));
}

/// 路径输入必须保留 endpoint 身份，并按当前主机的原生路径规则解析相对段。
///
/// 本测试使用平台提供的绝对临时路径，避免把 Windows 盘符路径误当成 Linux 本地绝对路径。
///
/// @author kongweiguang
#[test]
fn path_draft_preserves_endpoint_and_resolves_relative_segments() {
    let host_id = HostId::new();
    let remote = FileEndpoint::remote(host_id.clone(), "/srv/app/config").unwrap();
    let resolved = endpoint_for_path(&remote, "../logs/./today").unwrap();
    assert_eq!(
        resolved,
        FileEndpoint::remote(host_id, "/srv/app/logs/today").unwrap()
    );

    let local_root = std::env::temp_dir().join("kerminal-files-path-test");
    assert!(local_root.is_absolute());
    let local = FileEndpoint::Local {
        path: local_root.clone(),
    };
    assert_eq!(
        endpoint_for_path(&local, "assets").unwrap(),
        FileEndpoint::Local {
            path: local_root.join("assets"),
        }
    );
}

/// 容器路径以容器根为边界，绝对化并拒绝越根输入。
#[test]
fn container_path_draft_is_anchored_and_cannot_escape_root() {
    let endpoint = FileEndpoint::Container {
        host_id: None,
        runtime: ContainerRuntime::Podman,
        container_id: "web".to_owned(),
        path: RemotePath::parse_container("/app/config").unwrap(),
    };

    assert_eq!(
        endpoint_for_path(&endpoint, "../logs").unwrap(),
        FileEndpoint::Container {
            host_id: None,
            runtime: ContainerRuntime::Podman,
            container_id: "web".to_owned(),
            path: RemotePath::parse_container("/app/logs").unwrap(),
        }
    );
    assert!(endpoint_for_path(&endpoint, "../../../../etc").is_err());
}

/// 空路径和点文件分类在 UI 层明确处理，不依赖不存在的默认目录或后端权限猜测。
#[test]
fn path_draft_rejects_empty_input_and_hidden_filter_uses_dot_names() {
    let endpoint = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    assert!(endpoint_for_path(&endpoint, "  ").is_err());

    let entry = FileEntry {
        endpoint: endpoint.clone(),
        name: ".env".to_owned(),
        kind: FileEntryKind::File,
        size: Some(5),
        modified: None,
        permissions: None,
        is_hidden: false,
        raw: None,
    };
    assert!(is_hidden_entry(&entry));
}

/// 隐藏属性以来源 metadata 为准，不依赖 UI 猜测权限文本或原始列表行。
///
/// @author kongweiguang
#[test]
fn source_hidden_attribute_is_respected_for_non_dot_names() {
    let entry = FileEntry {
        endpoint: FileEndpoint::Local {
            path: PathBuf::from("C:\\work\\.hidden-attributes"),
        },
        name: "system-file".to_owned(),
        kind: FileEntryKind::File,
        size: Some(5),
        modified: None,
        permissions: None,
        is_hidden: true,
        raw: None,
    };

    assert!(is_hidden_entry(&entry));
}

/// Host A 的书签响应晚于活动侧切到 Host B 时必须失效，旧代次也不能覆盖新请求。
///
/// @author kongweiguang
#[test]
fn late_bookmark_response_for_old_host_is_rejected() {
    let host_a = HostId::new();
    let host_b = HostId::new();
    let endpoint_a = FileEndpoint::remote(host_a, "/srv/a").unwrap();
    let endpoint_b = FileEndpoint::remote(host_b, "/srv/b").unwrap();
    let target_a = bookmark_target(&endpoint_a).unwrap().0;

    assert!(!bookmark_response_is_current(4, 4, &target_a, &endpoint_b));
    assert!(!bookmark_response_is_current(3, 4, &target_a, &endpoint_a));
    assert!(bookmark_response_is_current(4, 4, &target_a, &endpoint_a));
}

/// 本机 Windows 的 mode 映射不能作为 ACL 编辑器输入；远端 POSIX metadata 可用时仍保留 chmod。
///
/// @author kongweiguang
#[test]
fn permission_editor_requires_posix_semantics_and_real_metadata() {
    let local = FileEntry {
        endpoint: FileEndpoint::Local {
            path: PathBuf::from("C:\\work\\README.md"),
        },
        name: "README.md".to_owned(),
        kind: FileEntryKind::File,
        size: Some(20),
        modified: None,
        permissions: Some(0o666),
        is_hidden: false,
        raw: None,
    };
    let editor = PermissionEditor::default();
    assert_eq!(
        supports_posix_mode_endpoint(&local.endpoint),
        cfg!(target_family = "unix")
    );
    assert_eq!(
        editor.can_show_editor(Some(&local)),
        cfg!(target_family = "unix")
    );
    #[cfg(windows)]
    assert!(editor.explanation(Some(&local)).contains("ACL"));

    let remote = FileEntry {
        endpoint: FileEndpoint::remote(HostId::new(), "/srv/README.md").unwrap(),
        name: "README.md".to_owned(),
        kind: FileEntryKind::File,
        size: Some(20),
        modified: None,
        permissions: Some(0o644),
        is_hidden: false,
        raw: None,
    };
    assert!(editor.can_show_editor(Some(&remote)));

    let mut unsupported = PermissionEditor::default();
    unsupported.mark_unsupported(remote.endpoint.clone());
    assert!(!unsupported.can_show_editor(Some(&remote)));
}

/// 新任务默认以 Rename 保留双方数据，用户选择仍通过传输偏好菜单更改。
///
/// @author kongweiguang
#[test]
fn transfer_conflict_preference_keeps_safe_rename_default() {
    assert_eq!(
        TransferQueueUi::default().conflict_policy,
        ConflictPolicy::Rename
    );
}

/// 单列排序只影响同类项目，目录优先级保持稳定且名称列不区分大小写。
///
/// @author kongweiguang
#[test]
fn file_sort_keeps_directories_first_and_orders_names_case_insensitively() {
    let endpoint = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let mut entries = [
        FileEntry {
            endpoint: endpoint.clone(),
            name: "beta".to_owned(),
            kind: FileEntryKind::File,
            size: Some(1),
            modified: None,
            permissions: None,
            is_hidden: false,
            raw: None,
        },
        FileEntry {
            endpoint: endpoint.clone(),
            name: "Alpha".to_owned(),
            kind: FileEntryKind::File,
            size: Some(1),
            modified: None,
            permissions: None,
            is_hidden: false,
            raw: None,
        },
        FileEntry {
            endpoint,
            name: "z-dir".to_owned(),
            kind: FileEntryKind::Directory,
            size: Some(999),
            modified: None,
            permissions: None,
            is_hidden: false,
            raw: None,
        },
    ];

    entries.sort_by(|left, right| compare_entries(left, right, FileSort::default()));

    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["z-dir", "Alpha", "beta"]
    );
}

/// 大小和时间列按实际数值排序，反向只改变同类型分组内的顺序。
///
/// @author kongweiguang
#[test]
fn file_sort_uses_numeric_metadata_and_reverses_selected_column() {
    let endpoint = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let mut entries = [
        FileEntry {
            endpoint: endpoint.clone(),
            name: "large".to_owned(),
            kind: FileEntryKind::File,
            size: Some(20),
            modified: Some("200".to_owned()),
            permissions: None,
            is_hidden: false,
            raw: None,
        },
        FileEntry {
            endpoint: endpoint.clone(),
            name: "small".to_owned(),
            kind: FileEntryKind::File,
            size: Some(2),
            modified: Some("100".to_owned()),
            permissions: None,
            is_hidden: false,
            raw: None,
        },
    ];

    entries.sort_by(|left, right| {
        compare_entries(
            left,
            right,
            FileSort {
                column: FileSortColumn::Size,
                descending: false,
            },
        )
    });
    assert_eq!(entries[0].name, "small");
    entries.sort_by(|left, right| {
        compare_entries(
            left,
            right,
            FileSort {
                column: FileSortColumn::Modified,
                descending: true,
            },
        )
    });
    assert_eq!(entries[0].name, "large");
}

/// 同列点击反转、切换列回到升序，避免每个列第一次激活就呈现意外降序。
///
/// @author kongweiguang
#[test]
fn sort_toggle_starts_a_new_column_in_ascending_order() {
    let mut sort = FileSort::default();
    sort.toggle(FileSortColumn::Name);
    assert!(sort.descending);
    sort.toggle(FileSortColumn::Size);
    assert_eq!(sort.column, FileSortColumn::Size);
    assert!(!sort.descending);
}

/// 筛选只按目录项种类投影，特殊文件保持在“全部”中且不会被错算成文件或目录。
///
/// @author kongweiguang
#[test]
fn file_entry_filter_keeps_special_items_in_all_only() {
    assert!(FileEntryFilter::All.matches(FileEntryKind::File));
    assert!(FileEntryFilter::All.matches(FileEntryKind::Directory));
    assert!(FileEntryFilter::All.matches(FileEntryKind::Other));
    assert!(FileEntryFilter::Files.matches(FileEntryKind::File));
    assert!(!FileEntryFilter::Files.matches(FileEntryKind::Directory));
    assert!(!FileEntryFilter::Files.matches(FileEntryKind::Other));
    assert!(FileEntryFilter::Directories.matches(FileEntryKind::Directory));
    assert!(!FileEntryFilter::Directories.matches(FileEntryKind::File));
    assert!(!FileEntryFilter::Directories.matches(FileEntryKind::Other));
}
