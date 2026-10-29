//! Root 容器面板目标选择契约；所有目标均由结构化 session/workspace 身份构造。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

use kerminal_application::{
    sessions::UiSessionBinding,
    workspace::{
        WorkspaceContainerRuntime, WorkspaceFileAccess, WorkspaceFileSource,
        WorkspaceLocalMachineScope, WorkspaceMachineStatus, WorkspacePaneMode,
        WorkspaceSessionPane, WorkspaceSessionTab, WorkspaceTargetRef,
    },
};
use kerminal_domain::{
    files::ContainerRuntime,
    identity::{HostId, PaneId, SessionId, TabId},
    sessions::{
        ContainerSessionTarget, LocalSessionTarget, SerialSessionTarget, SessionLifecycle,
        SessionOwner, SessionTarget, SshSessionTarget, TelnetSessionTarget,
    },
    workspace::{SplitDirection, TerminalLayoutNode},
};

use crate::{
    features::operations::{ContainerPanelContext, ContainerPanelUnavailableReason},
    root::{
        CenterPanel,
        container_panel::{
            ContainerPanelCandidate, ContainerPanelPane,
            container_target_for_validated_host_context, resolve_container_panel_context,
            workspace_file_host_context,
        },
        session_reconnect::SessionPanelObservation,
        workspace_runtime::RuntimePanePhase,
    },
};

/// 生成持久目标与 transient 语义均符合生产投影的 pane 数据。
struct PanelFixture {
    tab_id: TabId,
    pane_id: PaneId,
    session_id: SessionId,
    target: SessionTarget,
    persisted: WorkspaceSessionPane,
    snapshot_persisted: WorkspaceSessionPane,
    transient: bool,
    external_lease_host_id: Option<HostId>,
    handler_binding: UiSessionBinding,
    observation: SessionPanelObservation,
}

/// 默认保持完整真实绑定，只在指定测试中替换一个失配维度。
struct PanelOverrides<'a> {
    center_panel: CenterPanel,
    selected_tab_id: &'a TabId,
    focused_pane_id: &'a PaneId,
    handler_binding: Option<&'a UiSessionBinding>,
    observation: &'a SessionPanelObservation,
    persisted: &'a WorkspaceSessionPane,
    snapshot_persisted: &'a WorkspaceSessionPane,
    layout: Option<TerminalLayoutNode>,
}

impl<'a> PanelOverrides<'a> {
    /// 建立使用 fixture 全部权威身份的基线输入，测试只覆盖明确要变动的字段。
    ///
    /// @author kongweiguang
    fn valid(fixture: &'a PanelFixture) -> Self {
        Self {
            center_panel: CenterPanel::Terminal,
            selected_tab_id: &fixture.tab_id,
            focused_pane_id: &fixture.pane_id,
            handler_binding: Some(&fixture.handler_binding),
            observation: &fixture.observation,
            persisted: &fixture.persisted,
            snapshot_persisted: &fixture.snapshot_persisted,
            layout: None,
        }
    }
}

impl PanelFixture {
    /// 构造 service observation 和输入 handler 使用相同的真实 Ui owner/generation。
    ///
    /// @author kongweiguang
    fn new(target: SessionTarget, lifecycle: SessionLifecycle) -> Self {
        let tab_id = TabId::new();
        let pane_id = PaneId::new();
        let session_id = SessionId::new();
        let (persisted, transient) = persisted_pane(pane_id.clone(), &target);
        let binding = UiSessionBinding {
            session_id: session_id.clone(),
            owner: SessionOwner::Ui {
                tab_id: tab_id.clone(),
                pane_id: pane_id.clone(),
            },
            session_generation: 41,
        };
        let observation = SessionPanelObservation {
            binding: binding.clone(),
            target: target.clone(),
            lifecycle,
        };
        Self {
            tab_id,
            pane_id,
            session_id,
            target,
            snapshot_persisted: persisted.clone(),
            persisted,
            transient,
            external_lease_host_id: None,
            handler_binding: binding,
            observation,
        }
    }

    /// 通过生产 resolver 验证同一 focused pane，不使用测试专属目标推断逻辑。
    ///
    /// @author kongweiguang
    fn resolve(&self) -> ContainerPanelContext {
        self.resolve_with(PanelOverrides::valid(self))
    }

    /// 可替换一个身份维度以确认 resolver 在提交容器工具上下文前会拒绝失配。
    ///
    /// @author kongweiguang
    fn resolve_with(&self, overrides: PanelOverrides<'_>) -> ContainerPanelContext {
        let layout = overrides
            .layout
            .unwrap_or_else(|| TerminalLayoutNode::pane(self.pane_id.clone()));
        let active_tab = WorkspaceSessionTab::Terminal {
            id: self.tab_id.clone(),
            tab_group_id: None,
            title: "终端".into(),
            machine_id: "machine-local".into(),
            layout,
        };
        let pane = ContainerPanelPane {
            tab_id: &self.tab_id,
            pane_id: &self.pane_id,
            session_id: &self.session_id,
            target: &self.target,
            persisted: overrides.persisted,
            snapshot_persisted: overrides.snapshot_persisted,
            phase: RuntimePanePhase::Ready,
            transient: self.transient,
            external_lease_host_id: self.external_lease_host_id.as_ref(),
        };
        resolve_container_panel_context(ContainerPanelCandidate {
            center_panel: overrides.center_panel,
            active_tab_id: Some(overrides.selected_tab_id),
            active_tab: Some(&active_tab),
            focused_pane_id: Some(overrides.focused_pane_id),
            pane: Some(pane),
            handler_binding: overrides.handler_binding,
            observation: Some(overrides.observation),
        })
    }
}

/// 用与 Root workspace projection 相同的目标映射构造测试 pane。
///
/// @author kongweiguang
fn persisted_pane(pane_id: PaneId, target: &SessionTarget) -> (WorkspaceSessionPane, bool) {
    let mut persisted = WorkspaceSessionPane {
        id: pane_id,
        title: "终端".into(),
        machine_id: "machine-local".into(),
        mode: WorkspacePaneMode::Local,
        target: Some(WorkspaceTargetRef::Local { profile_id: None }),
        remote_host_id: None,
        container_id: None,
        profile_id: None,
        shell: None,
        args: None,
        cwd: None,
        current_cwd: None,
        env: None,
        remote_command: None,
        tmux_binding: None,
        prompt: String::new(),
        status: WorkspaceMachineStatus::Offline,
        latency_ms: None,
        local_machine_scope: Some(WorkspaceLocalMachineScope::Workspace),
        output_history: None,
    };
    let transient = match target {
        SessionTarget::Local(_) => false,
        SessionTarget::Ssh(ssh) => {
            persisted.mode = WorkspacePaneMode::Ssh;
            persisted.target = Some(WorkspaceTargetRef::Ssh {
                host_id: ssh.host_id.clone(),
            });
            persisted.remote_host_id = Some(ssh.host_id.clone());
            false
        }
        SessionTarget::Container(container) => {
            persisted.mode = WorkspacePaneMode::Container;
            persisted.container_id = Some(container.container_id.clone());
            persisted.remote_host_id = container.host_id.clone();
            if let Some(host_id) = container.host_id.as_ref() {
                persisted.target = Some(WorkspaceTargetRef::DockerContainer {
                    host_id: host_id.clone(),
                    container_id: container.container_id.clone(),
                    runtime: match container.runtime {
                        ContainerRuntime::Docker => WorkspaceContainerRuntime::Docker,
                        ContainerRuntime::Podman => WorkspaceContainerRuntime::Podman,
                    },
                    container_name: None,
                    user: None,
                    workdir: None,
                });
                false
            } else {
                persisted.target = None;
                persisted.machine_id = "machine-local-container".into();
                true
            }
        }
        SessionTarget::Telnet(_) => {
            persisted.mode = WorkspacePaneMode::Telnet;
            persisted.target = None;
            true
        }
        SessionTarget::Serial(_) => {
            persisted.mode = WorkspacePaneMode::Serial;
            persisted.target = None;
            true
        }
    };
    (persisted, transient)
}

/// 在不同本地配置下构造可区分的 SSH 主机目标。
///
/// @author kongweiguang
fn ssh_target(host_id: HostId) -> SessionTarget {
    SessionTarget::Ssh(SshSessionTarget {
        host_id,
        working_directory: None,
        command: None,
    })
}

/// 文件 Tab 测试输入保留正式 target 和 tab 身份，不经过终端 pane fixture。
fn workspace_file_tab(
    id: TabId,
    target: WorkspaceTargetRef,
    source: WorkspaceFileSource,
) -> WorkspaceSessionTab {
    WorkspaceSessionTab::WorkspaceFile {
        id,
        tab_group_id: None,
        title: "compose.yaml".into(),
        machine_id: "file-host".into(),
        target,
        path: "/srv/app/compose.yaml".into(),
        root_path: Some("/srv/app".into()),
        access: WorkspaceFileAccess::Readonly,
        source,
    }
}

/// Compose SSH 文件和容器文件均解析到真实 tab/host，结果不包含会话 binding。
#[test]
fn workspace_file_host_context_keeps_tab_and_host_identity() {
    let tab_id = TabId::new();
    let host_id = HostId::new();
    let ssh_file = workspace_file_tab(
        tab_id.clone(),
        WorkspaceTargetRef::Ssh {
            host_id: host_id.clone(),
        },
        WorkspaceFileSource::ComposeYaml,
    );
    assert_eq!(
        workspace_file_host_context(&ssh_file),
        Some(ContainerPanelContext::WorkspaceHost {
            tab_id: tab_id.clone(),
            host_id: host_id.clone(),
        })
    );

    let container_tab_id = TabId::new();
    let container_file = workspace_file_tab(
        container_tab_id.clone(),
        WorkspaceTargetRef::DockerContainer {
            host_id: host_id.clone(),
            container_id: "container-123".into(),
            runtime: WorkspaceContainerRuntime::Podman,
            container_name: None,
            user: None,
            workdir: None,
        },
        WorkspaceFileSource::Container,
    );
    assert_eq!(
        workspace_file_host_context(&container_file),
        Some(ContainerPanelContext::WorkspaceHost {
            tab_id: container_tab_id,
            host_id,
        })
    );
}

/// 本机和非 SSH 文件目标不借用当前侧栏主机来开启容器管理。
#[test]
fn workspace_file_without_ssh_host_has_no_container_host_context() {
    let local = workspace_file_tab(
        TabId::new(),
        WorkspaceTargetRef::Local { profile_id: None },
        WorkspaceFileSource::Local,
    );
    let telnet = workspace_file_tab(
        TabId::new(),
        WorkspaceTargetRef::Telnet {
            host_id: HostId::new(),
        },
        WorkspaceFileSource::Workspace,
    );

    assert_eq!(workspace_file_host_context(&local), None);
    assert_eq!(workspace_file_host_context(&telnet), None);
}

/// 以精确 host/runtime/container ID 构造容器终端目标。
///
/// @author kongweiguang
fn container_target(
    host_id: Option<HostId>,
    runtime: ContainerRuntime,
    container_id: &str,
) -> SessionTarget {
    SessionTarget::Container(ContainerSessionTarget {
        host_id,
        runtime,
        container_id: container_id.to_owned(),
        command: vec![OsString::from("sh")],
    })
}

/// workspace 无活动终端时明确返回 unavailable；切换中心呈现页不应重绑或清除真实活动 pane。
#[test]
fn center_navigation_preserves_workspace_terminal_binding() {
    let fixture = PanelFixture::new(
        SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("sh"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        SessionLifecycle::Running,
    );
    let mut connections_center = PanelOverrides::valid(&fixture);
    connections_center.center_panel = CenterPanel::Connections;
    assert_eq!(fixture.resolve(), fixture.resolve_with(connections_center));

    let no_pane = resolve_container_panel_context(ContainerPanelCandidate {
        center_panel: CenterPanel::Connections,
        active_tab_id: Some(&fixture.tab_id),
        active_tab: None,
        focused_pane_id: Some(&fixture.pane_id),
        pane: None,
        handler_binding: None,
        observation: None,
    });
    assert_eq!(
        no_pane,
        ContainerPanelContext::Unavailable {
            reason: ContainerPanelUnavailableReason::NoTerminal,
        }
    );

    let not_ready = resolve_container_panel_context(ContainerPanelCandidate {
        center_panel: CenterPanel::Files,
        active_tab_id: Some(&fixture.tab_id),
        active_tab: Some(&WorkspaceSessionTab::Terminal {
            id: fixture.tab_id.clone(),
            tab_group_id: None,
            title: "终端".into(),
            machine_id: "machine-local".into(),
            layout: TerminalLayoutNode::pane(fixture.pane_id.clone()),
        }),
        focused_pane_id: Some(&fixture.pane_id),
        pane: None,
        handler_binding: None,
        observation: None,
    });
    assert_eq!(
        not_ready,
        ContainerPanelContext::Unavailable {
            reason: ContainerPanelUnavailableReason::NotReady,
        }
    );
}

/// pane owner 和 handler generation 同时参与身份判断，session ID 单独匹配不足以授权读取。
#[test]
fn owner_generation_and_split_focus_must_match_exact_pane() {
    let fixture = PanelFixture::new(ssh_target(HostId::new()), SessionLifecycle::Running);
    let mut wrong_owner = fixture.observation.clone();
    wrong_owner.binding.owner = SessionOwner::Ui {
        tab_id: TabId::new(),
        pane_id: fixture.pane_id.clone(),
    };
    let mut wrong_owner_input = PanelOverrides::valid(&fixture);
    wrong_owner_input.observation = &wrong_owner;
    assert!(matches!(
        fixture.resolve_with(wrong_owner_input),
        ContainerPanelContext::Unavailable {
            reason: ContainerPanelUnavailableReason::TargetMismatch,
        }
    ));

    let mut changed_generation = fixture.handler_binding.clone();
    changed_generation.session_generation += 1;
    let mut changed_generation_input = PanelOverrides::valid(&fixture);
    changed_generation_input.handler_binding = Some(&changed_generation);
    assert!(matches!(
        fixture.resolve_with(changed_generation_input),
        ContainerPanelContext::Unavailable {
            reason: ContainerPanelUnavailableReason::TargetMismatch,
        }
    ));

    let other_pane_id = PaneId::new();
    let split_layout = TerminalLayoutNode::Split {
        id: "split-1".into(),
        direction: SplitDirection::Horizontal,
        children: vec![
            TerminalLayoutNode::pane(fixture.pane_id.clone()),
            TerminalLayoutNode::pane(other_pane_id.clone()),
        ],
        sizes: None,
    };
    let mut split_input = PanelOverrides::valid(&fixture);
    split_input.focused_pane_id = &other_pane_id;
    split_input.layout = Some(split_layout);
    assert!(matches!(
        fixture.resolve_with(split_input),
        ContainerPanelContext::Unavailable {
            reason: ContainerPanelUnavailableReason::TargetMismatch,
        }
    ));
}

/// 右栏仅接受明确 SSH 宿主，本机不会借用左栏主机或暴露 main 没有的容器入口。
#[test]
fn host_context_rejects_local_and_binds_exact_ssh_session() {
    let local = PanelFixture::new(
        SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("sh"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        SessionLifecycle::Running,
    );
    assert!(matches!(
        local.resolve(),
        ContainerPanelContext::Unavailable {
            reason: ContainerPanelUnavailableReason::UnsupportedProtocol,
        }
    ));

    let first_host = HostId::new();
    let second_host = HostId::new();
    let first = PanelFixture::new(ssh_target(first_host.clone()), SessionLifecycle::Running);
    let second = PanelFixture::new(ssh_target(second_host.clone()), SessionLifecycle::Running);
    assert!(matches!(
        first.resolve(),
        ContainerPanelContext::Host { host_id: Some(host_id), .. } if host_id == first_host
    ));
    assert!(matches!(
        second.resolve(),
        ContainerPanelContext::Host { host_id: Some(host_id), .. } if host_id == second_host
    ));
}

/// Docker 与 Podman 的容器终端只解析其 SSH 宿主，本机容器不构成右栏的后备目标。
#[test]
fn container_terminal_context_requires_remote_host() {
    for runtime in [ContainerRuntime::Docker, ContainerRuntime::Podman] {
        for host_id in [None, Some(HostId::new())] {
            let fixture = PanelFixture::new(
                container_target(host_id.clone(), runtime, "container-abc"),
                SessionLifecycle::Running,
            );
            match host_id {
                Some(host_id) => assert!(matches!(
                    fixture.resolve(),
                    ContainerPanelContext::Host { host_id: Some(resolved_host), .. }
                        if resolved_host == host_id
                )),
                None => assert!(matches!(
                    fixture.resolve(),
                    ContainerPanelContext::Unavailable {
                        reason: ContainerPanelUnavailableReason::UnsupportedProtocol,
                    }
                )),
            }
        }
    }
}

/// 远程容器 resolver 输出 Host context；Ports 从同一 binding 和 pane 还原精确容器目标。
///
/// @author kongweiguang
#[test]
fn remote_container_host_context_projects_exact_ports_target() {
    let host_id = HostId::new();
    let fixture = PanelFixture::new(
        container_target(
            Some(host_id.clone()),
            ContainerRuntime::Podman,
            "remote-container",
        ),
        SessionLifecycle::Running,
    );
    let context = fixture.resolve();
    assert!(matches!(
        &context,
        ContainerPanelContext::Host {
            binding,
            host_id: Some(resolved_host),
        } if binding == &fixture.handler_binding && resolved_host == &host_id
    ));

    let pane = ContainerPanelPane {
        tab_id: &fixture.tab_id,
        pane_id: &fixture.pane_id,
        session_id: &fixture.session_id,
        target: &fixture.target,
        persisted: &fixture.persisted,
        snapshot_persisted: &fixture.snapshot_persisted,
        phase: RuntimePanePhase::Ready,
        transient: fixture.transient,
        external_lease_host_id: fixture.external_lease_host_id.as_ref(),
    };
    let SessionTarget::Container(session_target) = &fixture.target else {
        unreachable!("fixture must represent a remote container terminal");
    };
    let expected = kerminal_domain::operations::ContainerTarget {
        host_id: session_target.host_id.clone(),
        runtime: session_target.runtime,
        container_id: session_target.container_id.clone(),
    };

    assert_eq!(
        container_target_for_validated_host_context(&context, pane),
        Some(expected)
    );
}

/// 容器退出后保留已确认的 SSH 宿主；本机与已关闭的会话不能新增右栏管理目标。
///
/// @author kongweiguang
#[test]
fn container_terminal_exit_keeps_host_context_but_closed_or_host_exit_is_unavailable() {
    for host_id in [None, Some(HostId::new())] {
        for lifecycle in [SessionLifecycle::Exited, SessionLifecycle::Failed] {
            let fixture = PanelFixture::new(
                container_target(host_id.clone(), ContainerRuntime::Podman, "retained"),
                lifecycle,
            );
            let mut exited_input = PanelOverrides::valid(&fixture);
            exited_input.handler_binding = None;
            match host_id.as_ref() {
                Some(host_id) => assert!(matches!(
                    fixture.resolve_with(exited_input),
                    ContainerPanelContext::Host { host_id: Some(resolved_host), .. }
                        if &resolved_host == host_id
                )),
                None => assert!(matches!(
                    fixture.resolve_with(exited_input),
                    ContainerPanelContext::Unavailable {
                        reason: ContainerPanelUnavailableReason::UnsupportedProtocol,
                    }
                )),
            }
        }
    }

    let host = PanelFixture::new(ssh_target(HostId::new()), SessionLifecycle::Exited);
    assert!(matches!(
        host.resolve(),
        ContainerPanelContext::Unavailable {
            reason: ContainerPanelUnavailableReason::TerminalEnded,
        }
    ));

    for lifecycle in [SessionLifecycle::Closing, SessionLifecycle::Closed] {
        let container = PanelFixture::new(
            container_target(None, ContainerRuntime::Docker, "ended"),
            lifecycle,
        );
        assert!(matches!(
            container.resolve(),
            ContainerPanelContext::Unavailable {
                reason: ContainerPanelUnavailableReason::TerminalEnded,
            }
        ));
    }
}

/// runtime 持久 pane 和 workspace snapshot 必须一致，容器目标/lease 变动不会沿用旧上下文。
#[test]
fn persisted_target_snapshot_and_external_lease_must_agree() {
    let remote_host = HostId::new();
    let mut fixture = PanelFixture::new(
        container_target(
            Some(remote_host.clone()),
            ContainerRuntime::Docker,
            "same-id",
        ),
        SessionLifecycle::Running,
    );
    let mut renamed_snapshot = fixture.snapshot_persisted.clone();
    renamed_snapshot.title = "动态重命名后的标题".into();
    renamed_snapshot.status = WorkspaceMachineStatus::Online;
    renamed_snapshot.cwd = Some("/tmp/current".into());
    assert!(matches!(
        fixture.resolve_with({
            let mut input = PanelOverrides::valid(&fixture);
            input.snapshot_persisted = &renamed_snapshot;
            input
        }),
        ContainerPanelContext::Host { host_id: Some(host_id), .. } if host_id == remote_host
    ));

    let mut divergent_snapshot = fixture.snapshot_persisted.clone();
    divergent_snapshot.container_id = Some("different-id".into());
    let mut divergent_input = PanelOverrides::valid(&fixture);
    divergent_input.snapshot_persisted = &divergent_snapshot;
    assert!(matches!(
        fixture.resolve_with(divergent_input),
        ContainerPanelContext::Unavailable {
            reason: ContainerPanelUnavailableReason::TargetMismatch,
        }
    ));

    let mut mismatched_runtime = fixture.persisted.clone();
    mismatched_runtime.target = Some(WorkspaceTargetRef::DockerContainer {
        host_id: HostId::new(),
        container_id: "same-id".into(),
        runtime: WorkspaceContainerRuntime::Podman,
        container_name: None,
        user: None,
        workdir: None,
    });
    fixture.snapshot_persisted = mismatched_runtime.clone();
    let mut mismatched_target_input = PanelOverrides::valid(&fixture);
    mismatched_target_input.persisted = &mismatched_runtime;
    mismatched_target_input.snapshot_persisted = &fixture.snapshot_persisted;
    assert!(matches!(
        fixture.resolve_with(mismatched_target_input),
        ContainerPanelContext::Unavailable {
            reason: ContainerPanelUnavailableReason::TargetMismatch,
        }
    ));

    let external_host =
        HostId::parse("external:panel-fixture").expect("fixture 应使用合法 external host id");
    let mut external =
        PanelFixture::new(ssh_target(external_host.clone()), SessionLifecycle::Running);
    assert!(matches!(
        external.resolve(),
        ContainerPanelContext::Unavailable {
            reason: ContainerPanelUnavailableReason::TargetMismatch,
        }
    ));
    external.external_lease_host_id = Some(external_host.clone());
    assert!(matches!(
        external.resolve(),
        ContainerPanelContext::Host { host_id: Some(host_id), .. } if host_id == external_host
    ));

    let mut external_container = PanelFixture::new(
        container_target(
            Some(external_host.clone()),
            ContainerRuntime::Docker,
            "external-container",
        ),
        SessionLifecycle::Running,
    );
    assert!(matches!(
        external_container.resolve(),
        ContainerPanelContext::Unavailable {
            reason: ContainerPanelUnavailableReason::TargetMismatch,
        }
    ));
    external_container.external_lease_host_id = Some(external_host.clone());
    assert!(matches!(
        external_container.resolve(),
        ContainerPanelContext::Host { host_id: Some(host_id), .. } if host_id == external_host
    ));
}

/// Telnet、串口和未完成生命周期不伪装成容器宿主或容器信息页。
#[test]
fn unsupported_protocol_and_non_ready_lifecycle_are_explicit() {
    let telnet = PanelFixture::new(
        SessionTarget::Telnet(TelnetSessionTarget {
            host: "example.invalid".into(),
            port: 23,
        }),
        SessionLifecycle::Running,
    );
    let serial = PanelFixture::new(
        SessionTarget::Serial(SerialSessionTarget::new(PathBuf::from("COM4"))),
        SessionLifecycle::Running,
    );
    for fixture in [&telnet, &serial] {
        assert!(matches!(
            fixture.resolve(),
            ContainerPanelContext::Unavailable {
                reason: ContainerPanelUnavailableReason::UnsupportedProtocol,
            }
        ));
    }

    let starting = PanelFixture::new(
        container_target(None, ContainerRuntime::Docker, "starting"),
        SessionLifecycle::Starting,
    );
    assert!(matches!(
        starting.resolve(),
        ContainerPanelContext::Unavailable {
            reason: ContainerPanelUnavailableReason::NotReady,
        }
    ));
}
