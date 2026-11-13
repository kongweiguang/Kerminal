//! 外部连接转发的纯类型映射测试。
//!
//! @author kongweiguang

use std::{cell::Cell, collections::BTreeMap, ffi::OsString, rc::Rc, sync::Arc};

use async_trait::async_trait;
use gpui_kit::{AppContext as _, TestAppContext, VisualTestContext};
use kerminal_application::{
    AppError, AppResult,
    connections::management::{
        HostAuthenticationMode, HostDocumentRevision, HostDraft, HostGroupDeletionReceipt,
        HostGroupDraft, HostGroupSnapshot, HostManagementRepository, HostManagementService,
        HostRouteSettings, HostSftpOptions, HostTerminalOptions, ManagedHost, RdpDisplayOptions,
    },
    plugins::{PluginId, PluginRegistry, PluginRegistryBuilder},
    workspace::{
        WorkspaceContainerRuntime, WorkspaceMachineStatus, WorkspacePaneMode,
        WorkspacePinnedContainer, WorkspaceSessionDocument, WorkspaceSessionPane,
        WorkspaceSessionSnapshot, WorkspaceSessionTab, WorkspaceTargetRef,
    },
};
use kerminal_domain::{
    connections::{HostEndpoint, RemoteProtocol},
    files::FileEndpoint,
    identity::{HostId, PaneId, SessionId, TabId},
    operations::{ContainerRuntime, ContainerTarget},
    preferences::ConfigRevision,
    sessions::{
        ContainerSessionTarget, LocalSessionTarget, SerialSessionTarget, SessionTarget,
        SshInitialCommand, SshSessionTarget, TelnetSessionTarget, TerminalSize,
    },
    workspace::TerminalLayoutNode,
};
use tokio_util::sync::CancellationToken;

use crate::features::connections::{ConnectionsEvent, HostNavigationEvent, HostSessionStatus};
use crate::features::files::{FilePaneSide, FileTargetNameLookup, FilesPane, SftpHostRequested};
use crate::features::workspace_controller::WorkspaceController;
use crate::root::{
    CenterPanel, RootView, TerminalTargetRequest,
    connections::{
        ExistingContainerSessionTarget, FileSftpOrigin, FileSftpSelectionRequest,
        PendingSftpHostCreation, SftpNavigationReceiptDisposition, SftpNavigationRequest,
        connect_target_protocol, consume_sftp_navigation_receipt, external_ssh_session_target,
        handle_connections_event, handle_host_navigation_event, local_profile_target_request,
        open_pinned_container, remove_pinned_container_snapshot,
        select_existing_container_session_target, select_pinned_container_snapshot,
        session_status_for_phase, sftp_host_creation_source_is_current,
        sftp_transfer_tab_activation, upsert_pinned_container_snapshot,
    },
    workspace_files::FilesWorkspaceSelection,
    workspace_runtime::{RuntimePanePhase, WorkspaceRuntime},
};

/// 连接前门禁只覆盖可禁用的传输协议；本地与容器保持原生命周期判定。
///
/// Root handler 在创建会话前调用同一映射，再查询注入后的冻结视图；禁用协议时只写入
/// 现有状态通道，不创建会话或启动进程。
///
/// @author kongweiguang
#[test]
fn connect_target_protocol_only_gates_protocol_targets() {
    let host_id = HostId::new();
    assert_eq!(
        connect_target_protocol(&SessionTarget::Ssh(SshSessionTarget {
            host_id: host_id.clone(),
            working_directory: None,
            command: None,
        })),
        Some(RemoteProtocol::Ssh)
    );
    assert_eq!(
        connect_target_protocol(&SessionTarget::Telnet(TelnetSessionTarget {
            host: "fixture.invalid".to_owned(),
            port: 23,
        })),
        Some(RemoteProtocol::Telnet)
    );
    assert_eq!(
        connect_target_protocol(&SessionTarget::Serial(SerialSessionTarget::new(
            "COM1".into()
        ))),
        Some(RemoteProtocol::Serial)
    );
    assert_eq!(
        connect_target_protocol(&SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("pwsh"),
            args: Vec::new(),
            working_directory: None,
            environment: Default::default(),
        })),
        None
    );
    assert_eq!(
        connect_target_protocol(&SessionTarget::Container(ContainerSessionTarget {
            host_id: Some(host_id),
            runtime: ContainerRuntime::Docker,
            container_id: "fixture".to_owned(),
            command: Vec::new(),
        })),
        None
    );
}

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
        origin: None,
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
/// 门禁只查询冻结启用视图，任何目录写入或读取都不应发生；实现保持失败以证明路径未进入。
struct GatedHostDirectory;

#[async_trait]
impl HostManagementRepository for GatedHostDirectory {
    async fn ensure_host_writable(&self) -> AppResult<()> {
        Err(AppError::unsupported("门禁回归不应访问主机目录"))
    }

    async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>> {
        Ok(Vec::new())
    }

    async fn load_host(&self, _: &HostId) -> AppResult<Option<ManagedHost>> {
        Ok(None)
    }

    async fn save_host(
        &self,
        _: HostId,
        _: HostDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost> {
        Err(AppError::unsupported("门禁回归不应保存主机"))
    }

    async fn delete_host(&self, _: &HostId, _: &ConfigRevision) -> AppResult<ManagedHost> {
        Err(AppError::unsupported("门禁回归不应删除主机"))
    }

    async fn load_groups(&self) -> AppResult<HostGroupSnapshot> {
        Ok(HostGroupSnapshot {
            groups: Vec::new(),
            revision: None,
        })
    }

    async fn save_group(
        &self,
        _: HostGroupDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot> {
        Err(AppError::unsupported("门禁回归不应保存分组"))
    }

    async fn delete_group_and_reassign(
        &self,
        _: String,
        _: Option<String>,
        _: ConfigRevision,
        _: Vec<HostDocumentRevision>,
    ) -> AppResult<HostGroupDeletionReceipt> {
        Err(AppError::unsupported("门禁回归不应执行分组事务"))
    }
}

/// 推进真实 GPUI 与 Tokio 回执，确保 Root 初始化完成后再触发入口。
fn settle(runtime: &tokio::runtime::Runtime, visual: &mut VisualTestContext) {
    for _ in 0..8 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
}

/// SFTP 禁用时工作台新建主机入口在捕获来源和打开模态前拒绝，原选择与工作区材料不变。
///
/// @author kongweiguang
#[gpui_kit::test]
fn disabled_sftp_workbench_entry_fails_without_opening_the_editor(app: &mut TestAppContext) {
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins().expect("内置声明应可注册");
    let states = std::collections::BTreeMap::from([(
        PluginId::new("kerminal.sftp").expect("插件 ID 应合法"),
        false,
    )]);
    let registry = Arc::new(
        builder
            .freeze_with_builtin_states(&states)
            .expect("内置状态应可冻结"),
    );
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, |services| services);
    settle(&runtime, visual);

    let (_file_runtime, files, transfers, executor, _calls) =
        crate::features::files::action_test_support::services();
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.services.hosts = Some(
                HostManagementService::new(Arc::new(GatedHostDirectory)).with_plugins(registry),
            );
            let pane = cx.new(|_| {
                FilesPane::new(
                    files,
                    transfers,
                    executor,
                    FileTargetNameLookup::default(),
                    FileEndpoint::Local {
                        path: "C:/fixture".into(),
                    },
                )
            });
            let started = root.begin_create_sftp_host_from_workbench(
                &pane,
                FilePaneSide::Right,
                1,
                window,
                cx,
            );
            assert!(!started, "禁用协议不能启动新建主机流程");
            assert!(!root.connections_modal_open, "失败时不得打开连接编辑器模态");
            assert!(
                root.pending_sftp_host_creation.is_none(),
                "失败时不得写入待保存回执"
            );
            assert!(
                root.active_file_sftp_selection.is_none(),
                "失败时不得占用文件选择请求"
            );
            assert!(
                root.status.as_ref().is_some_and(|status| {
                    status.contains("SFTP is disabled") || status.contains("SFTP 已禁用")
                }),
                "失败应复用现有状态反馈"
            );
        });
    });
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

/// 主机选择候选测试只返回内存快照；任何写操作都说明选择器越过了只读范围。
struct PickerDirectory {
    rows: Vec<ManagedHost>,
}

#[async_trait]
impl HostManagementRepository for PickerDirectory {
    async fn ensure_host_writable(&self) -> AppResult<()> {
        Err(AppError::unsupported("候选过滤不应访问主机目录写门禁"))
    }

    async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>> {
        Ok(self.rows.clone())
    }

    async fn load_host(&self, host_id: &HostId) -> AppResult<Option<ManagedHost>> {
        Ok(self.rows.iter().find(|host| &host.id == host_id).cloned())
    }

    async fn save_host(
        &self,
        _: HostId,
        _: HostDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost> {
        Err(AppError::unsupported("候选过滤不应保存主机"))
    }

    async fn delete_host(&self, _: &HostId, _: &ConfigRevision) -> AppResult<ManagedHost> {
        Err(AppError::unsupported("候选过滤不应删除主机"))
    }

    async fn load_groups(&self) -> AppResult<HostGroupSnapshot> {
        Ok(HostGroupSnapshot {
            groups: Vec::new(),
            revision: None,
        })
    }

    async fn save_group(
        &self,
        _: HostGroupDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot> {
        Err(AppError::unsupported("候选过滤不应保存分组"))
    }

    async fn delete_group_and_reassign(
        &self,
        _: String,
        _: Option<String>,
        _: ConfigRevision,
        _: Vec<HostDocumentRevision>,
    ) -> AppResult<HostGroupDeletionReceipt> {
        Err(AppError::unsupported("候选过滤不应执行分组事务"))
    }
}

/// 冻结测试插件视图；未列出的内置单元保持生产默认启用。
fn registry_with_disabled(disabled: &[&str]) -> Arc<PluginRegistry> {
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins().expect("内置声明应可注册");
    let states: BTreeMap<PluginId, bool> = disabled
        .iter()
        .map(|id| (PluginId::new(*id).expect("插件 ID 应合法"), false))
        .collect();
    Arc::new(
        builder
            .freeze_with_builtin_states(&states)
            .expect("内置状态应可冻结"),
    )
}

/// 候选过滤测试主机默认启用主机级 SFTP；只改变协议，避免测试复制另一套默认值。
fn picker_host(protocol: RemoteProtocol) -> ManagedHost {
    ManagedHost {
        id: HostId::new(),
        group_id: None,
        name: format!("{protocol:?} fixture"),
        note: None,
        protocol,
        endpoint: HostEndpoint {
            host: "fixture.invalid".to_owned(),
            port: 22,
            username: "tester".to_owned(),
        },
        authentication: None,
        authentication_mode: HostAuthenticationMode::Agent,
        rdp_password: None,
        route: HostRouteSettings::default(),
        serial: None,
        tags: Vec::new(),
        sort_order: 0,
        connect_timeout_ms: 30_000,
        keepalive_interval_ms: None,
        terminal_options: HostTerminalOptions::default(),
        sftp_options: HostSftpOptions::default(),
        rdp_display: RdpDisplayOptions::default(),
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
        revision: ConfigRevision::from_sha256("picker-fixture".to_owned()),
    }
}

/// 在空 v3 工作区中固定一个容器并清空选中态，使禁用路径能证明快照未被改写。
fn install_pinned_container(root: &mut RootView, host_id: &HostId) -> String {
    let mut controller = WorkspaceController::from_missing().expect("空 v3 基线应有效");
    let mut snapshot = controller.snapshot().clone();
    let pin = WorkspacePinnedContainer {
        target: ContainerTarget {
            host_id: Some(host_id.clone()),
            runtime: ContainerRuntime::Docker,
            container_id: "container-1".to_owned(),
        },
        name: "worker".to_owned(),
        description: String::new(),
        group_id: None,
    };
    let stable_id = pin.stable_id();
    assert!(upsert_pinned_container_snapshot(&mut snapshot, pin));
    snapshot.selected_machine_id = None;
    controller
        .synchronize_snapshot(snapshot)
        .expect("固定容器快照应可同步");
    root.workspace_controller = Some(controller);
    stable_id
}

/// 共享工作台入口是 SFTP 功能路径；SFTP 禁用时事件不得打开文件工作区。
///
/// @author kongweiguang
#[gpui_kit::test]
fn disabled_sftp_workbench_event_is_rejected(app: &mut TestAppContext) {
    let registry = registry_with_disabled(&["kerminal.sftp"]);
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, |services| services);
    settle(&runtime, visual);

    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.services.hosts = Some(
                HostManagementService::new(Arc::new(GatedHostDirectory)).with_plugins(registry),
            );
            handle_host_navigation_event(
                root,
                &HostNavigationEvent::OpenSftpWorkbenchRequested,
                window,
                cx,
            );
            assert_ne!(
                root.center_panel,
                CenterPanel::Files,
                "禁用时不得切换工作区"
            );
            assert!(!root.sftp_workbench_tab_open, "禁用时不得打开共享工作台");
            assert!(
                root.status.as_ref().is_some_and(|status| {
                    status.contains("SFTP is disabled") || status.contains("SFTP 已禁用")
                }),
                "拒绝应复用现有状态反馈"
            );
        });
    });
}

/// 面板与导航快照可能不同步；Root 在 complete 前复核 SFTP 单元，禁用时取消来源
/// 选择并推进队列，确保文件面板不会读取远端目录。
///
/// @author kongweiguang
#[gpui_kit::test]
fn disabled_sftp_selection_ready_is_cancelled_before_complete(app: &mut TestAppContext) {
    let registry = registry_with_disabled(&["kerminal.sftp"]);
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, |services| services);
    settle(&runtime, visual);

    let (_file_runtime, files, transfers, executor, _calls) =
        crate::features::files::action_test_support::services();
    let pane = visual.update(|_, app| {
        app.new(|_| {
            FilesPane::new(
                files,
                transfers,
                executor,
                FileTargetNameLookup::default(),
                FileEndpoint::Local {
                    path: "C:/fixture".into(),
                },
            )
        })
    });
    let generation = Rc::new(Cell::new(None));
    let captured = generation.clone();
    let subscription = visual.update(|_, app| {
        app.subscribe(&pane, move |_, event: &SftpHostRequested, _| {
            captured.set(Some(event.generation))
        })
    });
    // 订阅在下一轮效果刷新后才激活；先让 GPUI 完成挂载，再触发真实面板选择。
    visual.run_until_parked();
    let host_id = HostId::new();
    visual.update(|_, app| {
        pane.update(app, |pane, cx| pane.set_host_picker_enabled(true, cx));
        pane.update(app, |pane, cx| {
            crate::features::files::action_test_support::request_saved_host(
                pane,
                FilePaneSide::Left,
                host_id.clone(),
                cx,
            )
        });
    });
    let generation = generation.get().expect("面板应公布选择代次");

    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.services.hosts = Some(
                HostManagementService::new(Arc::new(GatedHostDirectory)).with_plugins(registry),
            );
            root.files_pane = Some(pane.clone());
            root.center_panel = CenterPanel::Files;
            root.sftp_workbench_tab_open = true;
            root.active_file_sftp_selection = Some(FileSftpSelectionRequest {
                request_id: "ready-defense".to_owned(),
                host_id: host_id.clone(),
                origin: FileSftpOrigin {
                    source: FilesWorkspaceSelection::Workbench,
                    pane_id: pane.entity_id().as_u64(),
                    side: FilePaneSide::Left,
                    generation,
                    navigation_generation: root.surface_navigation_generation,
                },
            });

            handle_connections_event(
                root,
                &ConnectionsEvent::SftpSelectionReady {
                    request_id: "ready-defense".to_owned(),
                    endpoint: FileEndpoint::remote(host_id, "/srv").expect("远端路径应有效"),
                    initial_local_directory: None,
                },
                window,
                cx,
            );

            assert!(root.active_file_sftp_selection.is_none());
            assert!(
                !pane
                    .read(cx)
                    .host_selection_is_current(FilePaneSide::Left, generation),
                "禁用回执必须取消来源选择，不得 complete"
            );
            assert!(
                root.status.as_ref().is_some_and(|status| {
                    status.contains("SFTP is disabled") || status.contains("SFTP 已禁用")
                }),
                "防御性拒绝应复用状态反馈"
            );
        });
    });
    drop(subscription);
}

/// 固定容器只有在新建/重开时才门禁 SSH 与 containers；禁用时不得改写选中快照
/// 或创建终端资源，已有会话复用路径仍保留原行为。
///
/// @author kongweiguang
#[gpui_kit::test]
fn pinned_container_open_rejects_disabled_ssh_or_containers(app: &mut TestAppContext) {
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, |services| services);
    settle(&runtime, visual);

    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let host_id = HostId::new();
            let stable_id = install_pinned_container(root, &host_id);

            let containers_registry = registry_with_disabled(&["kerminal.containers"]);
            root.services.plugins = containers_registry.clone();
            root.services.hosts = Some(
                HostManagementService::new(Arc::new(GatedHostDirectory))
                    .with_plugins(containers_registry),
            );
            open_pinned_container(root, &stable_id, window, cx);
            assert!(
                root.status.as_ref().is_some_and(|status| {
                    status.contains("containers plugin is disabled")
                        || status.contains("容器插件已禁用")
                }),
                "containers 禁用应给出明确状态"
            );
            assert_eq!(
                root.workspace_controller
                    .as_ref()
                    .expect("工作区控制器应存在")
                    .snapshot()
                    .selected_machine_id,
                None,
                "拒绝时不得修改选中快照"
            );
            assert!(
                root.workspace_runtime.panes().next().is_none(),
                "拒绝时不得创建容器终端"
            );

            let ssh_registry = registry_with_disabled(&["kerminal.ssh"]);
            root.services.plugins = ssh_registry.clone();
            root.services.hosts = Some(
                HostManagementService::new(Arc::new(GatedHostDirectory)).with_plugins(ssh_registry),
            );
            root.status = None;
            open_pinned_container(root, &stable_id, window, cx);
            assert!(
                root.status.as_ref().is_some_and(|status| {
                    status.contains("SSH is disabled") || status.contains("SSH 已禁用")
                }),
                "SSH 禁用应给出明确状态"
            );
            assert_eq!(
                root.workspace_controller
                    .as_ref()
                    .expect("工作区控制器应存在")
                    .snapshot()
                    .selected_machine_id,
                None,
                "拒绝时不得修改选中快照"
            );
            assert!(
                root.workspace_runtime.panes().next().is_none(),
                "拒绝时不得创建容器终端"
            );
        });
    });
}

/// 已有唯一容器会话在 SSH 与 containers 同时禁用时仍直接复用、不新建；新门禁只作用于
/// 新建/重开路径，避免插件停用截断正在运行的目标。
/// @author kongweiguang
#[gpui_kit::test]
fn pinned_container_open_reuses_existing_session_when_units_disabled(app: &mut TestAppContext) {
    let host = picker_host(RemoteProtocol::Ssh);
    let host_id = host.id.clone();
    let registry = registry_with_disabled(&["kerminal.ssh", "kerminal.containers"]);
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, move |mut services| {
            services.plugins = registry.clone();
            services.hosts = Some(
                HostManagementService::new(Arc::new(PickerDirectory { rows: vec![host] }))
                    .with_plugins(registry),
            );
            services
        });
    settle(&runtime, visual);

    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let container_id = "container-1".to_owned();
    let target = ContainerTarget {
        host_id: Some(host_id.clone()),
        runtime: ContainerRuntime::Docker,
        container_id: container_id.clone(),
    };
    let pin = WorkspacePinnedContainer {
        target: target.clone(),
        name: "worker".to_owned(),
        description: "image · running".to_owned(),
        group_id: None,
    };
    let stable_id = pin.stable_id();
    let snapshot = WorkspaceSessionSnapshot {
        pinned_containers: vec![pin],
        active_tab_id: Some(tab_id.clone()),
        focused_pane_id: Some(pane_id.clone()),
        selected_machine_id: None,
        terminal_tabs: vec![WorkspaceSessionTab::Terminal {
            id: tab_id.clone(),
            tab_group_id: None,
            title: "worker".to_owned(),
            machine_id: host_id.as_str().to_owned(),
            layout: TerminalLayoutNode::pane(pane_id.clone()),
        }],
        terminal_panes: vec![WorkspaceSessionPane {
            id: pane_id.clone(),
            title: "worker".to_owned(),
            machine_id: host_id.as_str().to_owned(),
            mode: WorkspacePaneMode::Container,
            target: Some(WorkspaceTargetRef::DockerContainer {
                host_id: host_id.clone(),
                container_id: container_id.clone(),
                runtime: WorkspaceContainerRuntime::Docker,
                container_name: None,
                user: None,
                workdir: None,
            }),
            remote_host_id: Some(host_id.clone()),
            container_id: Some(container_id.clone()),
            profile_id: None,
            shell: None,
            args: None,
            cwd: None,
            current_cwd: None,
            env: None,
            remote_command: None,
            tmux_binding: None,
            prompt: String::new(),
            status: WorkspaceMachineStatus::Online,
            latency_ms: None,
            local_machine_scope: None,
            output_history: None,
        }],
        tab_groups: BTreeMap::new(),
    };

    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let mut controller = WorkspaceController::from_missing().expect("空 v3 基线应有效");
            controller
                .synchronize_snapshot(snapshot.clone())
                .expect("容器会话快照应可同步");
            root.workspace_controller = Some(controller);
            root.workspace_runtime =
                WorkspaceRuntime::from_snapshot(&snapshot).expect("运行时应可恢复");
            root.workspace_runtime
                .install_session(
                    &pane_id,
                    SessionId::new(),
                    SessionTarget::Container(ContainerSessionTarget {
                        host_id: Some(host_id.clone()),
                        runtime: ContainerRuntime::Docker,
                        container_id: container_id.clone(),
                        command: Vec::new(),
                    }),
                    TerminalSize::default(),
                )
                .expect("唯一容器会话应可标记为 Ready");

            open_pinned_container(root, &stable_id, window, cx);

            assert_eq!(root.center_panel, CenterPanel::Terminal);
            assert_eq!(
                root.workspace_runtime.panes().count(),
                1,
                "复用路径不得新建终端"
            );
            let snapshot = root
                .workspace_controller
                .as_ref()
                .expect("工作区控制器应存在")
                .snapshot();
            assert_eq!(snapshot.active_tab_id.as_ref(), Some(&tab_id));
            assert_eq!(snapshot.focused_pane_id.as_ref(), Some(&pane_id));
        });
    });
}

/// 容器文件集中门禁必须早于“文件服务未配置”分支；用 `files_pane = None` 作为哨兵，
/// 通过状态文案判断实际命中的分支，避免为测试构造真实 FilesPane。本地容器不依赖 SSH，
/// 只按 containers 单元判定，放行后应自然落到服务缺失状态。
/// @author kongweiguang
#[gpui_kit::test]
fn container_file_gate_precedes_missing_service_and_keeps_local_containers(
    app: &mut TestAppContext,
) {
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, |services| services);
    settle(&runtime, visual);

    let host_id = HostId::new();
    let remote_target = ContainerTarget {
        host_id: Some(host_id),
        runtime: ContainerRuntime::Docker,
        container_id: "container-1".to_owned(),
    };

    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.files_pane = None;

            let containers_disabled = registry_with_disabled(&["kerminal.containers"]);
            root.services.plugins = containers_disabled.clone();
            root.services.hosts = Some(
                HostManagementService::new(Arc::new(GatedHostDirectory))
                    .with_plugins(containers_disabled),
            );
            root.status = None;
            crate::root::operations::open_container_files(root, &remote_target, "/", window, cx);
            assert!(
                root.status.as_ref().is_some_and(|status| {
                    status.contains("容器插件已禁用")
                        || status.contains("containers plugin is disabled")
                }),
                "containers 门禁应早于文件服务缺失分支"
            );
            assert!(root.files_pane.is_none());

            let ssh_disabled = registry_with_disabled(&["kerminal.ssh"]);
            root.services.plugins = ssh_disabled.clone();
            root.services.hosts = Some(
                HostManagementService::new(Arc::new(GatedHostDirectory)).with_plugins(ssh_disabled),
            );
            root.status = None;
            crate::root::operations::open_container_files(root, &remote_target, "/", window, cx);
            assert!(
                root.status.as_ref().is_some_and(|status| {
                    status.contains("SSH 已禁用") || status.contains("SSH is disabled")
                }),
                "SSH 门禁应早于文件服务缺失分支"
            );
            assert!(root.files_pane.is_none());

            let enabled = registry_with_disabled(&[]);
            root.services.plugins = enabled.clone();
            root.services.hosts = Some(
                HostManagementService::new(Arc::new(GatedHostDirectory)).with_plugins(enabled),
            );
            root.status = None;
            crate::root::operations::open_container_files(root, &remote_target, "/", window, cx);
            assert!(
                root.status.as_ref().is_some_and(|status| {
                    status.contains("SSH 主机已不存在或协议已变化，无法打开容器文件")
                        || status.contains("The SSH host no longer exists or its protocol changed")
                }),
                "启用状态下导航缺少 host 时应拒绝容器文件请求"
            );
            assert!(root.files_pane.is_none());

            let local_target = ContainerTarget {
                host_id: None,
                runtime: ContainerRuntime::Docker,
                container_id: "local-container".to_owned(),
            };
            root.status = None;
            crate::root::operations::open_container_files(root, &local_target, "/", window, cx);
            assert!(
                root.status.as_ref().is_some_and(|status| {
                    status.contains("容器文件服务尚未配置")
                        || status.contains("Container file service is unavailable")
                }),
                "本地容器应越过协议门禁，落到文件服务缺失分支"
            );
            assert!(root.files_pane.is_none());
        });
    });
}

/// 固定项文件入口复用同一 containers 门禁；禁用时不得创建 pane，主机缺失时保留原
/// SSH 主机状态。测试只验证状态与哨兵，不构造真实文件服务。
/// @author kongweiguang
#[gpui_kit::test]
fn pinned_container_files_gate_blocks_before_any_pane(app: &mut TestAppContext) {
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, |services| services);
    settle(&runtime, visual);

    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let host_id = HostId::new();
            let stable_id = install_pinned_container(root, &host_id);
            root.files_pane = None;

            let containers_disabled = registry_with_disabled(&["kerminal.containers"]);
            root.services.plugins = containers_disabled.clone();
            root.services.hosts = Some(
                HostManagementService::new(Arc::new(GatedHostDirectory))
                    .with_plugins(containers_disabled),
            );
            root.status = None;
            crate::root::sidebar_containers::open_pinned_files(root, &stable_id, window, cx);
            assert!(
                root.status.as_ref().is_some_and(|status| {
                    status.contains("容器插件已禁用")
                        || status.contains("containers plugin is disabled")
                }),
                "固定项入口应在创建 pane 前拒绝禁用的 containers"
            );
            assert!(root.files_pane.is_none());
            assert!(
                root.workspace_runtime.panes().next().is_none(),
                "拒绝时不得创建终端 pane"
            );

            let enabled = registry_with_disabled(&[]);
            root.services.plugins = enabled.clone();
            root.services.hosts = Some(
                HostManagementService::new(Arc::new(GatedHostDirectory)).with_plugins(enabled),
            );
            root.status = None;
            crate::root::sidebar_containers::open_pinned_files(root, &stable_id, window, cx);
            assert!(
                root.status.as_ref().is_some_and(|status| {
                    status.contains("固定容器的 SSH 主机已不存在或协议已变化")
                        || status.contains("SSH host no longer exists or its protocol changed")
                }),
                "导航缺少 host 时应保留既有固定项状态"
            );
            assert!(root.files_pane.is_none());
            assert!(
                root.workspace_runtime.panes().next().is_none(),
                "主机缺失时不得创建终端 pane"
            );
        });
    });
}

/// containers 启用且导航中存在有效 SSH 主机时，固定项文件入口必须放行 wrapper 并到达
/// 共享 `open_container_files` 的 files 服务检查；用 `files_pane = None` 作哨兵，以状态
/// 文案区分共享入口的文件服务缺失分支和 wrapper 再次拦截。
/// @author kongweiguang
#[gpui_kit::test]
fn pinned_container_files_reach_shared_entry_when_enabled_and_host_present(
    app: &mut TestAppContext,
) {
    let host_id = HostId::new();
    let mut row = picker_host(RemoteProtocol::Ssh);
    row.id = host_id.clone();
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, move |mut services| {
            services.hosts = Some(
                HostManagementService::new(Arc::new(PickerDirectory { rows: vec![row] }))
                    .with_plugins(registry_with_disabled(&[])),
            );
            services
        });
    settle(&runtime, visual);

    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let stable_id = install_pinned_container(root, &host_id);
            root.files_pane = None;
            root.status = None;
            assert!(
                root.host_navigation
                    .as_ref()
                    .is_some_and(|navigation| navigation
                        .read(cx)
                        .ssh_host_group_id(&host_id)
                        .is_some()),
                "前置条件：导航真实 reload 后应提供有效 SSH 主机"
            );

            crate::root::sidebar_containers::open_pinned_files(root, &stable_id, window, cx);

            assert!(
                root.status.as_ref().is_some_and(|status| {
                    status.contains("容器文件服务尚未配置")
                        || status.contains("Container file service is unavailable.")
                }),
                "放行后应到达共享入口的文件服务缺失分支，而不是被 wrapper 再次拦截"
            );
            assert!(root.files_pane.is_none(), "服务缺失时不得构造文件面板");
            assert!(
                root.workspace_runtime.panes().next().is_none(),
                "服务缺失时不得创建终端 pane"
            );
        });
    });
}

/// 主机选择候选与面板预检共用冻结视图：SFTP 关闭时整体为空，SSH 主机还要求
/// SSH 启用；本地项和离线装配不在该列表内，保持原有行为。
///
/// @author kongweiguang
#[test]
fn sftp_picker_candidates_follow_protocol_enablement() {
    let ssh = picker_host(RemoteProtocol::Ssh);
    let sftp = picker_host(RemoteProtocol::Sftp);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("测试运行时应可创建");
    let cases: [(Vec<&str>, usize); 3] = [
        (Vec::new(), 2),
        (vec!["kerminal.ssh"], 1),
        (vec!["kerminal.sftp"], 0),
    ];
    for (disabled, expected) in cases {
        let registry = registry_with_disabled(&disabled);
        let hosts = HostManagementService::new(Arc::new(PickerDirectory {
            rows: vec![ssh.clone(), sftp.clone()],
        }))
        .with_plugins(registry);
        let lookup = FileTargetNameLookup::new(Some(hosts), None);
        let directory = runtime
            .block_on(lookup.list_sftp_hosts(CancellationToken::new()))
            .expect("隔离目录应可读取");
        assert_eq!(directory.hosts.len(), expected, "disabled: {disabled:?}");
    }
}
