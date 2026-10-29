//! Workspace file restore tests cover typed target mapping without real filesystem or network IO.
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString, path::Path};

use kerminal_application::workspace::{
    WorkspaceContainerRuntime, WorkspaceFileAccess, WorkspaceFileSource, WorkspaceSessionPane,
    WorkspaceSessionSnapshot, WorkspaceSessionTab, WorkspaceTargetRef,
    WorkspaceTargetUnavailableReason,
};
use kerminal_domain::{
    files::{ContainerRuntime, FileEndpoint, RemotePath},
    identity::{HostId, TabId},
};

use crate::features::editor::{EditorTabProjection, EditorWorkspaceProjection};
use crate::root::workspace_files::{
    FilesWorkspaceSelection, WorkspaceFileRestoreIssue, WorkspaceFileRestoreState,
    WorkspaceFileRuntime, editor_projection_contains_endpoint, sftp_transfer_endpoints,
    sftp_transfer_tab_for_host, should_return_to_terminal_after_editor_close,
    workspace_file_action_endpoint, workspace_file_endpoint_parts, workspace_file_full_path,
};
use crate::root::{
    CenterPanel, TerminalTargetRequest, is_generated_terminal_pane_title, terminal_tab_title,
};

/// 关闭当前传输页后跟随 reducer 的后继传输标签，不能继续引用已释放的面板。
///
/// @author kongweiguang
#[test]
fn closing_selected_transfer_follows_confirmed_surviving_tab() {
    let closed_id = TabId::new();
    let survivor_id = TabId::new();
    let host_id = HostId::new();
    let tab = sftp_transfer_tab_for_host(
        survivor_id.clone(),
        host_id,
        None,
        &RemotePath::parse("/srv").expect("valid path"),
        "transfer",
    );
    let mut selection = FilesWorkspaceSelection::TransferTab(closed_id.clone());
    selection.reconcile_removed_tab(&closed_id, Some(&snapshot_with_tabs(vec![tab])));
    assert_eq!(selection, FilesWorkspaceSelection::TransferTab(survivor_id));
}

/// 关闭最后一个传输页或后继是编辑页时回到共享工作台，不把缺失面板误报为服务故障。
///
/// @author kongweiguang
#[test]
fn closing_selected_transfer_falls_back_to_workbench_without_transfer_successor() {
    let closed_id = TabId::new();
    for snapshot in [
        snapshot_with_tabs(vec![]),
        snapshot_with_tabs(vec![local_workspace_file(
            TabId::new(),
            WorkspaceFileAccess::Editable,
        )]),
    ] {
        let mut selection = FilesWorkspaceSelection::TransferTab(closed_id.clone());
        selection.reconcile_removed_tab(&closed_id, Some(&snapshot));
        assert_eq!(selection, FilesWorkspaceSelection::Workbench);
    }
}

/// 后台标签的关闭回执不能把用户后来打开的共享工作台或其它传输页抢走。
///
/// @author kongweiguang
#[test]
fn closing_background_transfer_preserves_explicit_files_surface() {
    let closed_id = TabId::new();
    for selected in [
        FilesWorkspaceSelection::Workbench,
        FilesWorkspaceSelection::TransferTab(TabId::new()),
    ] {
        let mut selection = selected.clone();
        selection.reconcile_removed_tab(&closed_id, Some(&snapshot_with_tabs(vec![])));
        assert_eq!(selection, selected);
    }
}

/// 文件操作必须解析同 ID 的 Ready endpoint；Pending、错误 ID 或快照目标漂移均拒绝。
///
/// @author kongweiguang
#[test]
fn workspace_file_action_requires_exact_ready_tab_endpoint() {
    let id = TabId::parse("file-tab-ready").expect("valid file tab id");
    let tab = local_workspace_file(id.clone(), WorkspaceFileAccess::Editable);
    let endpoint = FileEndpoint::Local {
        path: "C:/workspace/notes.txt".into(),
    };
    let mut runtime = WorkspaceFileRuntime::from_snapshot(&snapshot_with_tabs(vec![tab.clone()]));
    assert_eq!(
        workspace_file_action_endpoint(&id, Some(&tab), &runtime),
        None
    );

    runtime.set_result(&id, Ok(Some(endpoint.clone())));
    assert_eq!(
        workspace_file_action_endpoint(&id, Some(&tab), &runtime),
        Some(endpoint.clone())
    );

    let other_id = TabId::new();
    assert_eq!(
        workspace_file_action_endpoint(&other_id, Some(&tab), &runtime),
        None
    );
    let mut changed_tab = tab.clone();
    if let WorkspaceSessionTab::WorkspaceFile { path, .. } = &mut changed_tab {
        *path = "C:/workspace/renamed.txt".to_owned();
    }
    assert_eq!(
        workspace_file_action_endpoint(&id, Some(&changed_tab), &runtime),
        None
    );
}

/// 本地/远端复制只包含文件路径，不泄漏主机 ID 或其它 endpoint 元数据。
///
/// @author kongweiguang
#[test]
fn workspace_file_full_path_copies_path_without_target_identity() {
    let local = FileEndpoint::Local {
        path: "C:/workspace/notes.txt".into(),
    };
    assert_eq!(
        workspace_file_full_path(&local).as_deref(),
        Some("C:/workspace/notes.txt")
    );
    let remote = FileEndpoint::Remote {
        host_id: HostId::parse("ssh-host-1").expect("valid test host id"),
        path: RemotePath::parse("/srv/app/notes.txt").expect("valid remote path"),
    };
    assert_eq!(
        workspace_file_full_path(&remote).as_deref(),
        Some("/srv/app/notes.txt")
    );
}

/// 默认 Profile 标题只使用缓存的已加载显示名，未就绪时保留本地化回退；SSH 仍按 HostId 查名。
///
/// @author kongweiguang
#[test]
fn terminal_tab_title_uses_localized_default_and_explicit_host_alias() {
    assert_eq!(
        terminal_tab_title(&TerminalTargetRequest::ProfileDefaultLocal, None),
        crate::locale::text("本地终端", "Local terminal")
    );

    let profile_title = terminal_tab_title(
        &TerminalTargetRequest::ProfileDefaultLocal,
        Some(gpui_kit::SharedString::from("PowerShell 7")),
    );
    assert_eq!(profile_title.to_string(), "PowerShell 7");

    let host_id = HostId::parse("ssh-host-1").expect("valid test host id");
    let target = TerminalTargetRequest::ExplicitHost {
        host_id: host_id.clone(),
        target: kerminal_domain::sessions::SessionTarget::Ssh(
            kerminal_domain::sessions::SshSessionTarget {
                host_id,
                working_directory: None,
                command: None,
            },
        ),
    };
    assert_eq!(
        terminal_tab_title(&target, Some(gpui_kit::SharedString::from("Fixture host"))).to_string(),
        "Fixture host"
    );
}

/// 自动本地标签名不能覆盖 pane 的 shell 标题，真正的用户改名仍按自定义名显示。
///
/// @author kongweiguang
#[test]
fn localized_local_tab_title_keeps_shell_pane_title() {
    let target = kerminal_domain::sessions::SessionTarget::Local(
        kerminal_domain::sessions::LocalSessionTarget {
            program: OsString::from("pwsh.exe"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        },
    );

    assert!(is_generated_terminal_pane_title("本地终端", Some(&target)));
    assert!(is_generated_terminal_pane_title(
        "Local terminal",
        Some(&target)
    ));
    assert!(!is_generated_terminal_pane_title(
        "Shell tools",
        Some(&target)
    ));
}

/// 创建最小 typed workspace 快照，避免测试依赖 JSON codec 或磁盘 fixture。
fn snapshot_with_tabs(tabs: Vec<WorkspaceSessionTab>) -> WorkspaceSessionSnapshot {
    WorkspaceSessionSnapshot {
        pinned_containers: Vec::new(),
        active_tab_id: tabs.first().map(|tab| tab.id().clone()),
        focused_pane_id: None,
        selected_machine_id: None,
        terminal_tabs: tabs,
        terminal_panes: Vec::<WorkspaceSessionPane>::new(),
        tab_groups: BTreeMap::new(),
    }
}

/// 创建本地工作区文件 DTO，集中保持测试使用的目标与来源契约一致。
fn local_workspace_file(id: TabId, access: WorkspaceFileAccess) -> WorkspaceSessionTab {
    WorkspaceSessionTab::WorkspaceFile {
        id,
        tab_group_id: None,
        title: "notes".to_owned(),
        machine_id: "local".to_owned(),
        target: WorkspaceTargetRef::Local { profile_id: None },
        path: "C:/workspace/notes.txt".to_owned(),
        root_path: None,
        access,
        source: WorkspaceFileSource::Local,
    }
}

/// YAML 新入口必须继承工作区只读策略，重复登记不能替换既有资源描述。
///
/// @author kongweiguang
#[test]
fn new_compose_yaml_tab_retains_readonly_policy_and_rejects_duplicate_registration() {
    let mut runtime = WorkspaceFileRuntime::from_snapshot(&snapshot_with_tabs(Vec::new()));
    let id = TabId::new();
    let mut tab = local_workspace_file(id.clone(), WorkspaceFileAccess::Readonly);
    if let WorkspaceSessionTab::WorkspaceFile { source, .. } = &mut tab {
        *source = WorkspaceFileSource::ComposeYaml;
    }
    assert!(runtime.register_pending_compose_yaml_tab(tab.clone()));
    assert!(!runtime.register_pending_compose_yaml_tab(tab));
    assert_eq!(
        runtime.restore_state(&id),
        Some(&WorkspaceFileRestoreState::Pending)
    );
    assert!(
        runtime.workspace_file_requires_readonly(&FileEndpoint::Local {
            path: "C:/workspace/notes.txt".into(),
        })
    );
}

/// Compose 专用入口不能被可写文件或一般文件别名绕过，不在快照安装前丢失资源归属。
///
/// @author kongweiguang
#[test]
fn compose_yaml_registration_rejects_editable_or_non_compose_descriptors() {
    let mut runtime = WorkspaceFileRuntime::from_snapshot(&snapshot_with_tabs(Vec::new()));
    let mut editable = local_workspace_file(TabId::new(), WorkspaceFileAccess::Editable);
    if let WorkspaceSessionTab::WorkspaceFile { source, .. } = &mut editable {
        *source = WorkspaceFileSource::ComposeYaml;
    }
    assert!(!runtime.register_pending_compose_yaml_tab(editable));
    assert!(
        !runtime.register_pending_compose_yaml_tab(local_workspace_file(
            TabId::new(),
            WorkspaceFileAccess::Readonly,
        ))
    );
    assert!(runtime.pending_tab_ids().is_empty());
}

/// 映射规则保留 SSH 主机身份和绝对 POSIX 路径，不从标签 machineId 猜测目标。
#[test]
fn workspace_file_ssh_target_maps_to_remote_endpoint() {
    let host_id = HostId::parse("ssh-host-1").expect("valid test host id");
    let endpoint = workspace_file_endpoint_parts(
        &WorkspaceTargetRef::Ssh {
            host_id: host_id.clone(),
        },
        "/srv/app/config.toml",
        WorkspaceFileSource::Sftp,
    )
    .expect("valid SSH file target");

    assert_eq!(
        endpoint,
        FileEndpoint::Remote {
            host_id,
            path: RemotePath::parse("/srv/app/config.toml").expect("valid remote path"),
        }
    );
}

/// 容器路径映射同时保留已保存的 runtime 与容器 ID，不隐式改成 Docker。
#[test]
fn workspace_file_container_target_preserves_podman_runtime() {
    let host_id = HostId::parse("ssh-host-1").expect("valid test host id");
    let endpoint = workspace_file_endpoint_parts(
        &WorkspaceTargetRef::DockerContainer {
            host_id: host_id.clone(),
            container_id: "container-7".to_owned(),
            runtime: WorkspaceContainerRuntime::Podman,
            container_name: Some("api".to_owned()),
            user: None,
            workdir: None,
        },
        "/etc/app/config.toml",
        WorkspaceFileSource::Container,
    )
    .expect("valid container file target");

    assert_eq!(
        endpoint,
        FileEndpoint::Container {
            host_id: Some(host_id),
            runtime: ContainerRuntime::Podman,
            container_id: "container-7".to_owned(),
            path: RemotePath::parse("/etc/app/config.toml").expect("valid container path"),
        }
    );
}

/// 无法安全解析的目标和临时 external 主机不得静默回退到本地或已选主机。
///
/// @author kongweiguang
#[test]
fn unavailable_and_external_targets_are_rejected() {
    let unavailable = WorkspaceTargetRef::Unavailable {
        raw: serde_json::json!({ "kind": "ssh" }),
        reason: WorkspaceTargetUnavailableReason::InvalidHostId,
    };
    assert_eq!(
        workspace_file_endpoint_parts(&unavailable, "/tmp/notes.txt", WorkspaceFileSource::Sftp,),
        Err(WorkspaceFileRestoreIssue::Unavailable(
            WorkspaceTargetUnavailableReason::InvalidHostId
        ))
    );

    assert_eq!(
        workspace_file_endpoint_parts(
            &WorkspaceTargetRef::Telnet {
                host_id: HostId::parse("telnet-host-1").expect("valid test host id"),
            },
            "/tmp/notes.txt",
            WorkspaceFileSource::Sftp,
        ),
        Err(WorkspaceFileRestoreIssue::UnsupportedTarget)
    );

    let external_id = HostId::parse("external:session-1").expect("valid opaque test id");
    assert_eq!(
        workspace_file_endpoint_parts(
            &WorkspaceTargetRef::Ssh {
                host_id: external_id,
            },
            "/tmp/notes.txt",
            WorkspaceFileSource::Sftp,
        ),
        Err(WorkspaceFileRestoreIssue::ExternalTarget)
    );
}

/// SFTP 双面板恢复严格使用锁定左主机、右目录与初始选中项的持久化值。
#[test]
fn sftp_transfer_restores_both_endpoints_and_selection() {
    let previous_left = HostId::parse("previous-left").expect("valid test host id");
    let locked_left = HostId::parse("locked-left").expect("valid test host id");
    let right_host = HostId::parse("right-host").expect("valid test host id");
    let tab = WorkspaceSessionTab::SftpTransfer {
        id: TabId::new(),
        tab_group_id: None,
        title: "SFTP".to_owned(),
        machine_id: "legacy-machine".to_owned(),
        left_host_id: Some(previous_left),
        locked_left_host_id: Some(locked_left.clone()),
        right_host_id: Some(right_host.clone()),
        external_launch_id: None,
        initial_left_path: None,
        initial_right_path: Some("/srv/releases".to_owned()),
        initial_right_selection: Some("release.tar".to_owned()),
    };

    let endpoints =
        sftp_transfer_endpoints(&tab, Path::new("C:/workspace")).expect("persisted SFTP endpoints");

    assert_eq!(
        endpoints.left,
        FileEndpoint::Remote {
            host_id: locked_left.clone(),
            path: RemotePath::parse("/").expect("root path"),
        }
    );
    assert_eq!(
        endpoints.right,
        FileEndpoint::Remote {
            host_id: right_host,
            path: RemotePath::parse("/srv/releases").expect("right path"),
        }
    );
    assert_eq!(endpoints.locked_left_host_id, Some(locked_left));
    assert_eq!(
        endpoints.initial_right_selection.as_deref(),
        Some("release.tar")
    );
}

/// 新建 SFTP 标签固定目标主机、远端路径和本地起始目录，并建立独立待恢复运行态。
///
/// @author kongweiguang
#[test]
fn new_host_transfer_tab_preserves_exact_host_path_and_pending_tab_identity() {
    let tab_id = TabId::parse("new-transfer-tab").expect("valid tab id");
    let host_id = HostId::parse("requested-ssh-host").expect("valid host id");
    let remote_path = RemotePath::parse("/srv/releases").expect("valid remote path");
    let tab = sftp_transfer_tab_for_host(
        tab_id.clone(),
        host_id.clone(),
        Some("C:/Users/example/Downloads".to_owned()),
        &remote_path,
        "Transfer · Build host",
    );
    let WorkspaceSessionTab::SftpTransfer {
        right_host_id,
        initial_left_path,
        initial_right_path,
        left_host_id,
        locked_left_host_id,
        external_launch_id,
        ..
    } = &tab
    else {
        panic!("new host transfer must use the existing SftpTransfer descriptor");
    };
    assert_eq!(right_host_id.as_ref(), Some(&host_id));
    assert_eq!(
        initial_left_path.as_deref(),
        Some("C:/Users/example/Downloads")
    );
    assert_eq!(initial_right_path.as_deref(), Some("/srv/releases"));
    assert!(left_host_id.is_none());
    assert!(locked_left_host_id.is_none());
    assert!(external_launch_id.is_none());

    let existing_id = TabId::parse("existing-transfer-tab").expect("valid existing tab id");
    let existing = sftp_transfer_tab_for_host(
        existing_id.clone(),
        HostId::parse("existing-ssh-host").expect("valid existing host id"),
        None,
        &RemotePath::parse("/").expect("valid root path"),
        "Existing transfer",
    );
    let mut runtime = WorkspaceFileRuntime::from_snapshot(&snapshot_with_tabs(vec![existing]));
    assert!(runtime.register_pending_sftp_transfer_tab(tab.clone()));
    assert_eq!(
        runtime.pending_tab_ids(),
        vec![existing_id.clone(), tab_id.clone()]
    );
    assert_eq!(
        runtime.restore_state(&tab_id),
        Some(&WorkspaceFileRestoreState::Pending)
    );
    assert!(!runtime.register_pending_sftp_transfer_tab(tab));
    assert_eq!(runtime.pending_tab_ids(), vec![existing_id, tab_id]);
}

/// SFTP 左右端都恢复各自快照路径；外部 launch 与全无目标的 tab 明确失败。
#[test]
fn sftp_transfer_local_path_and_ephemeral_target_are_handled_explicitly() {
    let local_tab = WorkspaceSessionTab::SftpTransfer {
        id: TabId::new(),
        tab_group_id: None,
        title: "Local transfer".to_owned(),
        machine_id: "legacy-machine".to_owned(),
        left_host_id: None,
        locked_left_host_id: None,
        right_host_id: Some(HostId::parse("right-host").expect("valid test host id")),
        external_launch_id: None,
        initial_left_path: Some("C:/Users/alice/Downloads".to_owned()),
        initial_right_path: Some("C:/downloads".to_owned()),
        initial_right_selection: None,
    };
    let local = sftp_transfer_endpoints(&local_tab, Path::new("C:/workspace"))
        .expect("local left endpoint and remote right endpoint");
    assert_eq!(
        local.left,
        FileEndpoint::Local {
            path: "C:/Users/alice/Downloads".into()
        }
    );

    let local_pair_tab = WorkspaceSessionTab::SftpTransfer {
        id: TabId::new(),
        tab_group_id: None,
        title: "Local transfer".to_owned(),
        machine_id: "sftp-transfer".to_owned(),
        left_host_id: None,
        locked_left_host_id: None,
        right_host_id: None,
        external_launch_id: None,
        initial_left_path: None,
        initial_right_path: None,
        initial_right_selection: None,
    };
    let local_pair = sftp_transfer_endpoints(&local_pair_tab, Path::new("C:/workspace"))
        .expect("hostless transfer workspace uses its two local panels");
    assert_eq!(
        local_pair.left,
        FileEndpoint::Local {
            path: "C:/workspace".into()
        }
    );
    assert_eq!(
        local_pair.right,
        FileEndpoint::Local {
            path: "C:/workspace".into()
        }
    );

    let external_tab = WorkspaceSessionTab::SftpTransfer {
        id: TabId::new(),
        tab_group_id: None,
        title: "Transient".to_owned(),
        machine_id: "external:session-1".to_owned(),
        left_host_id: None,
        locked_left_host_id: None,
        right_host_id: None,
        external_launch_id: Some("launch-1".to_owned()),
        initial_left_path: None,
        initial_right_path: None,
        initial_right_selection: None,
    };
    assert_eq!(
        sftp_transfer_endpoints(&external_tab, Path::new("C:/workspace")),
        Err(WorkspaceFileRestoreIssue::ExternalTarget)
    );
}

/// 只读策略在同 endpoint 的多个工作区标签间取严格值，不能由可编辑别名放宽。
#[test]
fn duplicate_workspace_file_endpoint_keeps_readonly_alias() {
    let editable_id = TabId::new();
    let readonly_id = TabId::new();
    let snapshot = snapshot_with_tabs(vec![
        local_workspace_file(editable_id, WorkspaceFileAccess::Editable),
        local_workspace_file(readonly_id, WorkspaceFileAccess::Readonly),
    ]);
    let runtime = WorkspaceFileRuntime::from_snapshot(&snapshot);
    let endpoint = FileEndpoint::Local {
        path: "C:/workspace/notes.txt".into(),
    };

    assert!(runtime.workspace_file_requires_readonly(&endpoint));
}

/// 快照只允許首次安裝；未完成 Root 关闭门禁前拒绝替换并保留原 TabId 映射。
#[test]
fn snapshot_replacement_is_rejected_without_close_gate() {
    let first_id = TabId::new();
    let second_id = TabId::new();
    let first_snapshot = snapshot_with_tabs(vec![local_workspace_file(
        first_id.clone(),
        WorkspaceFileAccess::Editable,
    )]);
    let second_snapshot = snapshot_with_tabs(vec![local_workspace_file(
        second_id,
        WorkspaceFileAccess::Editable,
    )]);
    let mut runtime = WorkspaceFileRuntime::default();

    runtime
        .install_snapshot(&first_snapshot)
        .expect("initial load may install the snapshot");
    assert!(runtime.install_snapshot(&second_snapshot).is_err());
    assert_eq!(runtime.pending_tab_ids(), vec![first_id]);
}

/// 失败项只有收到显式重试后才重新进入 Pending，避免 render 驱动无限重连。
#[test]
fn retry_changes_failed_tab_back_to_pending() {
    let tab_id = TabId::new();
    let snapshot = snapshot_with_tabs(vec![local_workspace_file(
        tab_id.clone(),
        WorkspaceFileAccess::Editable,
    )]);
    let mut runtime = WorkspaceFileRuntime::from_snapshot(&snapshot);
    runtime.set_result(
        &tab_id,
        Err((
            crate::features::workspace_controller::WorkspaceRestoreFailure::ResourceFailure,
            "temporarily unavailable".into(),
        )),
    );

    assert!(runtime.retry_failed_tab(&tab_id));
    assert_eq!(
        runtime.restore_state(&tab_id),
        Some(&WorkspaceFileRestoreState::Pending)
    );
}

/// Editor 行回执只接受仍打开的 endpoint，迟到的 Files 点击不能重新打开已关闭文件。
///
/// @author kongweiguang
#[test]
fn editor_activation_projection_rejects_closed_endpoint() {
    let open_endpoint = FileEndpoint::Local {
        path: "C:/workspace/open.txt".into(),
    };
    let closed_endpoint = FileEndpoint::Local {
        path: "C:/workspace/closed.txt".into(),
    };
    let projection = EditorWorkspaceProjection {
        revision: 1,
        tabs: vec![EditorTabProjection {
            endpoint: open_endpoint.clone(),
            title: "open.txt".to_owned(),
            dirty: false,
            readonly: false,
            loading: false,
            active: true,
        }],
        active_endpoint: Some(open_endpoint.clone()),
    };

    assert!(editor_projection_contains_endpoint(
        &projection,
        &open_endpoint
    ));
    assert!(!editor_projection_contains_endpoint(
        &projection,
        &closed_endpoint
    ));
    assert!(!should_return_to_terminal_after_editor_close(
        CenterPanel::Editor,
        &projection,
        true,
    ));
}

/// 最后一个临时文件关闭恢复原终端，但空初始投影、其他页面及文件型标签都不能抢占导航。
///
/// @author kongweiguang
#[test]
fn empty_editor_projection_returns_only_to_the_existing_selected_terminal() {
    let projection = EditorWorkspaceProjection {
        revision: 2,
        tabs: Vec::new(),
        active_endpoint: None,
    };
    assert!(should_return_to_terminal_after_editor_close(
        CenterPanel::Editor,
        &projection,
        true,
    ));
    assert!(!should_return_to_terminal_after_editor_close(
        CenterPanel::Connections,
        &projection,
        true,
    ));
    assert!(!should_return_to_terminal_after_editor_close(
        CenterPanel::Editor,
        &projection,
        false,
    ));
}
