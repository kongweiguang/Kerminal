//! 外部连接转发的纯类型映射测试。
//!
//! @author kongweiguang

use kerminal_application::workspace::{WorkspacePinnedContainer, WorkspaceSessionDocument};
use kerminal_domain::{
    identity::{HostId, PaneId, TabId},
    operations::{ContainerRuntime, ContainerTarget},
    sessions::{SessionTarget, SshInitialCommand},
};

use crate::features::connections::HostSessionStatus;
use crate::root::{
    TerminalTargetRequest,
    connections::{
        ExistingContainerSessionTarget, PendingSftpHostCreation, SftpNavigationReceiptDisposition,
        SftpNavigationRequest, consume_sftp_navigation_receipt, external_ssh_session_target,
        local_profile_target_request, remove_pinned_container_snapshot,
        select_existing_container_session_target, select_pinned_container_snapshot,
        session_status_for_phase, sftp_host_creation_source_is_current,
        sftp_transfer_tab_activation, upsert_pinned_container_snapshot,
    },
    workspace_runtime::RuntimePanePhase,
};

/// 同一主机的旧操作和用户切页后的成功都不能抢当前页面；确认模态没有中央导航则可继续。
///
/// @author kongweiguang
#[test]
fn sftp_preflight_receipt_requires_original_request_and_navigation() {
    let request = SftpNavigationRequest {
        request_id: "request-current".to_owned(),
        navigation_generation: 8,
    };
    assert!(request.accepts("request-current", 8));
    assert!(!request.accepts("request-old", 8));
    assert!(!request.accepts("request-current", 9));
}

/// 被新预检替代的旧回执不得消费新 request，也不得把最新预检的状态改成过期提示。
///
/// @author kongweiguang
#[test]
fn stale_sftp_receipt_preserves_newer_request_and_duplicates_are_ignored() {
    let mut pending = Some(SftpNavigationRequest {
        request_id: "request-new".to_owned(),
        navigation_generation: 9,
    });

    assert_eq!(
        consume_sftp_navigation_receipt(&mut pending, Some("request-new"), "request-old", 9,),
        SftpNavigationReceiptDisposition::Superseded
    );
    assert_eq!(
        pending.as_ref().map(|request| request.request_id.as_str()),
        Some("request-new")
    );
    assert_eq!(
        consume_sftp_navigation_receipt(&mut pending, Some("request-new"), "request-new", 9,),
        SftpNavigationReceiptDisposition::Accepted
    );
    assert_eq!(
        consume_sftp_navigation_receipt(&mut pending, Some("request-new"), "request-new", 9,),
        SftpNavigationReceiptDisposition::Missing
    );
}

/// 用户显式创建的独立传输标签在切页后仍加入持久工作区，但只在原页面时激活。
///
/// @author kongweiguang
#[test]
fn sftp_transfer_result_activates_only_when_navigation_is_unchanged() {
    assert_eq!(
        sftp_transfer_tab_activation(SftpNavigationReceiptDisposition::Accepted),
        Some(true)
    );
    assert_eq!(
        sftp_transfer_tab_activation(SftpNavigationReceiptDisposition::NavigationChanged),
        Some(false)
    );
    assert_eq!(
        sftp_transfer_tab_activation(SftpNavigationReceiptDisposition::Superseded),
        None
    );
    assert_eq!(
        sftp_transfer_tab_activation(SftpNavigationReceiptDisposition::Missing),
        None
    );
}

/// 新建主机保存回投同时绑定 flow、临时共享 Tab 身份和导航代次，切页或重建视图都会失效。
///
/// @author kongweiguang
#[test]
fn sftp_host_creation_return_requires_original_workbench_source() {
    let workbench_tab_id = TabId::new();
    let pending = PendingSftpHostCreation {
        request_id: "picker-flow".to_owned(),
        workbench_tab_id: workbench_tab_id.clone(),
        navigation_generation: 12,
        saved_host: Some((HostId::new(), true)),
    };

    assert!(sftp_host_creation_source_is_current(
        &pending,
        "picker-flow",
        &workbench_tab_id,
        12,
        true,
    ));
    assert!(!sftp_host_creation_source_is_current(
        &pending,
        "other-flow",
        &workbench_tab_id,
        12,
        true,
    ));
    assert!(!sftp_host_creation_source_is_current(
        &pending,
        "picker-flow",
        &TabId::new(),
        12,
        true,
    ));
    assert!(!sftp_host_creation_source_is_current(
        &pending,
        "picker-flow",
        &workbench_tab_id,
        13,
        true,
    ));
    assert!(!sftp_host_creation_source_is_current(
        &pending,
        "picker-flow",
        &workbench_tab_id,
        12,
        false,
    ));
}

/// 无命令的 SSH 外部请求保留交互式 shell 语义。
#[test]
fn empty_external_command_keeps_interactive_shell() {
    let host_id = HostId::new();
    let target =
        external_ssh_session_target(&host_id, &[]).expect("interactive SSH target is valid");

    assert!(matches!(
        target,
        SessionTarget::Ssh(target) if target.host_id == host_id && target.command.is_none()
    ));
}

/// 外部程序和参数逐项映射，空格与 shell 元字符仍留在各自 argv 元素内。
#[test]
fn external_ssh_command_preserves_argv_boundaries() {
    let host_id = HostId::new();
    let argv = vec![
        "printf".to_owned(),
        "two words".to_owned(),
        "$HOME; whoami".to_owned(),
    ];
    let target = external_ssh_session_target(&host_id, &argv).expect("structured command is valid");

    assert!(matches!(
        target,
        SessionTarget::Ssh(target)
            if target.command == Some(SshInitialCommand::Process {
                program: "printf".to_owned(),
                args: vec!["two words".to_owned(), "$HOME; whoami".to_owned()],
            })
    ));
}

/// 不接受没有程序名的非空 argv，避免启动时被误当成默认 shell。
#[test]
fn external_ssh_command_rejects_empty_program() {
    let host_id = HostId::new();
    let error = external_ssh_session_target(&host_id, &[String::new()])
        .expect_err("nonempty argv requires an executable name");

    assert_eq!(error.public_message(), "外部 SSH 命令缺少可执行程序");
}

/// 本地主机详情与左栏双击共用 ProfileId，避免统一目录时误转成远端 HostId。
/// @author kongweiguang
#[test]
fn local_profile_connect_preserves_stable_profile_identity() {
    let request = local_profile_target_request("local-profile-7");

    assert!(matches!(
        request,
        TerminalTargetRequest::ProfileLocal { profile_id } if profile_id == "local-profile-7"
    ));
}

/// pane 关闭阶段不再显示为活动连接，只有资源关闭失败才保留失败提示。
#[test]
fn host_projection_does_not_report_closing_panes_as_ready() {
    assert_eq!(
        session_status_for_phase(RuntimePanePhase::RestorePending),
        Some(HostSessionStatus::Opening)
    );
    assert_eq!(
        session_status_for_phase(RuntimePanePhase::Ready),
        Some(HostSessionStatus::Ready)
    );
    assert_eq!(
        session_status_for_phase(RuntimePanePhase::RestoreFailed),
        Some(HostSessionStatus::Failed)
    );
    assert_eq!(
        session_status_for_phase(RuntimePanePhase::CloseFailed),
        Some(HostSessionStatus::Failed)
    );
    assert_eq!(session_status_for_phase(RuntimePanePhase::Closing), None);
}

/// 固定同一稳定容器身份会更新展示信息；选择和移除不会改动终端标签或焦点。
/// @author kongweiguang
#[test]
fn pinned_container_snapshot_updates_selection_without_switching_tabs() {
    let host_id = HostId::new();
    let target = ContainerTarget {
        host_id: Some(host_id),
        runtime: ContainerRuntime::Docker,
        container_id: "container-1".to_owned(),
    };
    let mut snapshot = WorkspaceSessionDocument::empty_v3()
        .workspace_snapshot()
        .unwrap();
    let pin = WorkspacePinnedContainer {
        target: target.clone(),
        name: "worker".to_owned(),
        description: "image · running".to_owned(),
        group_id: Some("production".to_owned()),
    };
    let stable_id = pin.stable_id();

    assert!(upsert_pinned_container_snapshot(&mut snapshot, pin.clone()));
    assert_eq!(snapshot.pinned_containers.len(), 1);
    assert_eq!(
        snapshot.selected_machine_id.as_deref(),
        Some(stable_id.as_str())
    );
    assert_eq!(snapshot.active_tab_id, None);
    assert_eq!(snapshot.focused_pane_id, None);

    let mut renamed = pin;
    renamed.name = "worker-new".to_owned();
    assert!(upsert_pinned_container_snapshot(&mut snapshot, renamed));
    assert_eq!(snapshot.pinned_containers.len(), 1);
    assert_eq!(snapshot.pinned_containers[0].name, "worker-new");
    assert!(!select_pinned_container_snapshot(&mut snapshot, &stable_id));

    assert!(remove_pinned_container_snapshot(&mut snapshot, &stable_id));
    assert!(snapshot.pinned_containers.is_empty());
    assert_eq!(snapshot.selected_machine_id, None);
    assert_eq!(snapshot.active_tab_id, None);
    assert_eq!(snapshot.focused_pane_id, None);
    assert!(!remove_pinned_container_snapshot(&mut snapshot, &stable_id));
}

/// 固定容器的自动跳转要求唯一 TabId/PaneId；多个匹配 pane 时不猜焦点。
/// @author kongweiguang
#[test]
fn existing_container_session_activation_requires_a_unique_target() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();

    assert_eq!(
        select_existing_container_session_target(Vec::new()),
        ExistingContainerSessionTarget::Missing
    );
    assert_eq!(
        select_existing_container_session_target(vec![(tab_id.clone(), pane_id.clone())]),
        ExistingContainerSessionTarget::Unique {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        }
    );
    assert_eq!(
        select_existing_container_session_target(vec![
            (tab_id.clone(), pane_id),
            (tab_id, PaneId::new())
        ]),
        ExistingContainerSessionTarget::Multiple
    );
}

/// 保存重命名和取消共用返回规则；显式离开连接页后迟到回执不得夺回中心工作区。
///
/// @author kongweiguang
#[test]
fn group_rename_return_restores_only_while_connections_remain_active() {
    use crate::root::CenterPanel;

    assert_eq!(
        super::group_rename_return_target(CenterPanel::Connections, Some(CenterPanel::Terminal)),
        Some(CenterPanel::Terminal)
    );
    assert_eq!(
        super::group_rename_return_target(CenterPanel::Connections, Some(CenterPanel::Connections)),
        Some(CenterPanel::Connections)
    );
    assert_eq!(
        super::group_rename_return_target(CenterPanel::Files, Some(CenterPanel::Terminal)),
        None
    );
    assert_eq!(
        super::group_rename_return_target(CenterPanel::Connections, None),
        None
    );
}
