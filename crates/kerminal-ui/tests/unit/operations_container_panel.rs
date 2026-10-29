//! 固定容器面板只读范围与异步回执目标测试。
//!
//! @author kongweiguang

use super::super::render::container_panel::{
    ComposeProjectTab, ContainerHostGroupMode, ContainerHostPage, ContainerHostPresentation,
    HostContainerTab,
};
use super::{
    ContainerPanelContext, ContainerPanelUnavailableReason, host_context_matches_target,
    same_container_info_identity, same_read_context_scope,
};
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    identity::{HostId, PaneId, SessionId, TabId},
    operations::{ComposeProjectTarget, ContainerRuntime, ContainerTarget},
    sessions::SessionOwner,
};

/// 固定一致的 UI owner，使读请求范围测试能分别改变运行边沿、target 和 binding。
fn binding() -> UiSessionBinding {
    UiSessionBinding {
        session_id: SessionId::new(),
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
        session_generation: 7,
    }
}

/// 文件 Tab 的宿主上下文只带稳定 workspace 身份，不需要会话绑定。
fn workspace_host_context(tab_id: TabId, host_id: HostId) -> ContainerPanelContext {
    ContainerPanelContext::WorkspaceHost { tab_id, host_id }
}

/// 不可用状态文案保持与 main 一致，不暗示额外的切换或重连操作。
///
/// @author kongweiguang
#[test]
fn unavailable_reason_labels_describe_state_without_extra_actions() {
    assert_eq!(
        ContainerPanelUnavailableReason::NoTerminal.label(),
        (
            "当前没有可用于容器管理的 SSH 主机。",
            "No SSH host is available for container management."
        )
    );
    assert_eq!(
        ContainerPanelUnavailableReason::TerminalEnded.label(),
        (
            "当前 SSH 主机连接已结束。",
            "The SSH host connection has ended."
        )
    );
    assert_eq!(
        ContainerPanelUnavailableReason::TargetMismatch.label(),
        (
            "当前终端与容器 SSH 主机不匹配。",
            "The active terminal does not match the container SSH host."
        )
    );
    assert_eq!(
        ContainerPanelUnavailableReason::UnsupportedProtocol.label(),
        (
            "容器仅支持 SSH 主机上的 Docker 和 Podman。",
            "Containers are supported only on SSH hosts using Docker and Podman."
        )
    );
}

/// 终端 running 状态改变只刷新提示，同一容器在途读仍应完成且保留快照。
#[test]
fn terminal_running_edge_keeps_same_read_scope() {
    let target = ContainerTarget {
        host_id: Some(HostId::new()),
        runtime: ContainerRuntime::Podman,
        container_id: "container-123".to_owned(),
    };
    let running = ContainerPanelContext::ContainerInfo {
        binding: binding(),
        target: target.clone(),
        terminal_running: true,
    };
    let ended = ContainerPanelContext::ContainerInfo {
        binding: match &running {
            ContainerPanelContext::ContainerInfo { binding, .. } => binding.clone(),
            _ => unreachable!(),
        },
        target: target.clone(),
        terminal_running: false,
    };

    assert!(same_container_info_identity(&running, &ended));
    assert!(same_read_context_scope(Some(&ended), Some(&running)));
    assert!(!same_read_context_scope(Some(&running), None));
}

/// Runtime 与 host 必须同时命中当前宿主上下文，避免旧行被投到新运行时。
#[test]
fn host_context_matches_only_bound_host_and_runtime() {
    let host_id = HostId::new();
    let context = ContainerPanelContext::Host {
        binding: binding(),
        host_id: Some(host_id.clone()),
    };
    let target = ContainerTarget {
        host_id: Some(host_id.clone()),
        runtime: ContainerRuntime::Docker,
        container_id: "container-456".to_owned(),
    };

    assert!(host_context_matches_target(
        &context,
        Some(&host_id),
        ContainerRuntime::Docker,
        &target,
    ));
    assert!(!host_context_matches_target(
        &context,
        Some(&host_id),
        ContainerRuntime::Podman,
        &target,
    ));
    assert!(!host_context_matches_target(
        &context,
        None,
        ContainerRuntime::Docker,
        &target,
    ));
}

/// 同宿主跨文件 Tab 或终端/文件边界可复用只读结果，但不同宿主不能共享回执。
#[test]
fn workspace_host_reuses_reads_by_host_without_weakening_write_identity() {
    let host_id = HostId::new();
    let first = workspace_host_context(TabId::new(), host_id.clone());
    let second = workspace_host_context(TabId::new(), host_id.clone());
    let other_host = workspace_host_context(TabId::new(), HostId::new());
    let terminal = ContainerPanelContext::Host {
        binding: binding(),
        host_id: Some(host_id.clone()),
    };

    assert!(same_read_context_scope(Some(&first), Some(&second)));
    assert!(same_read_context_scope(Some(&terminal), Some(&first)));
    assert!(!same_read_context_scope(Some(&first), Some(&other_host)));
    assert_ne!(first, second);
}

/// WorkspaceHost 只接受同宿主且同 runtime 的容器目标，tab 精确性由操作票据另行校验。
#[test]
fn workspace_host_matches_only_its_host_and_runtime() {
    let host_id = HostId::new();
    let context = workspace_host_context(TabId::new(), host_id.clone());
    let target = ContainerTarget {
        host_id: Some(host_id.clone()),
        runtime: ContainerRuntime::Docker,
        container_id: "container-789".into(),
    };

    assert!(host_context_matches_target(
        &context,
        Some(&host_id),
        ContainerRuntime::Docker,
        &target,
    ));
    assert!(!host_context_matches_target(
        &context,
        Some(&HostId::new()),
        ContainerRuntime::Docker,
        &target,
    ));
    assert!(!host_context_matches_target(
        &context,
        Some(&host_id),
        ContainerRuntime::Podman,
        &target,
    ));
}

/// 为分组投影构造完整 Compose 身份，确保测试不依赖项目展示名推导目标。
///
/// @author kongweiguang
fn compose_target(
    host_id: Option<HostId>,
    project: &str,
    working_dir: &str,
) -> ComposeProjectTarget {
    ComposeProjectTarget {
        host_id,
        runtime: ContainerRuntime::Docker,
        project: project.to_owned(),
        working_dir: working_dir.to_owned(),
        config_files: vec![format!("{working_dir}/compose.yaml")],
    }
}

/// 新面板与 main 保持 Compose 分组及包含停止容器默认值，详情初始为空。
///
/// @author kongweiguang
#[test]
fn host_presentation_starts_with_main_defaults() {
    let presentation = ContainerHostPresentation::default();

    assert!(presentation.include_stopped());
    assert_eq!(presentation.group_mode(), ContainerHostGroupMode::Compose);
    assert_eq!(presentation.page(), &ContainerHostPage::List);
}

/// 每次从列表选择 Compose 项目都回到 Overview，与主版项目导航状态一致。
///
/// @author kongweiguang
#[test]
fn selecting_a_project_resets_its_tab_to_overview() {
    let first = compose_target(Some(HostId::new()), "web", "/srv/web");
    let second = compose_target(first.host_id.clone(), "web", "/srv/other-web");
    let mut presentation = ContainerHostPresentation::default();

    presentation.select_project(first.clone());
    presentation.set_project_tab(ComposeProjectTab::Yaml);
    presentation.select_project(first.clone());
    assert_eq!(
        presentation.page(),
        &ContainerHostPage::ComposeProject {
            target: first.clone()
        }
    );

    presentation.select_project(second.clone());
    assert_eq!(
        presentation.page(),
        &ContainerHostPage::ComposeProject { target: second }
    );
    assert_eq!(presentation.project_tab(), ComposeProjectTab::Overview);
    presentation.set_project_tab(ComposeProjectTab::Yaml);
    presentation.navigate_to_list();
    presentation.select_project(first.clone());
    assert_eq!(
        presentation.page(),
        &ContainerHostPage::ComposeProject { target: first }
    );
    assert_eq!(presentation.project_tab(), ComposeProjectTab::Overview);
}

/// 普通容器入口不覆盖当前 tab，旧目标的详情数据由请求 target 单独隔离。
///
/// @author kongweiguang
#[test]
fn container_navigation_keeps_tab_without_an_override() {
    let host_id = Some(HostId::new());
    let first = ContainerTarget {
        host_id: host_id.clone(),
        runtime: ContainerRuntime::Docker,
        container_id: "container-a".to_owned(),
    };
    let second = ContainerTarget {
        host_id,
        runtime: ContainerRuntime::Docker,
        container_id: "container-b".to_owned(),
    };
    let mut presentation = ContainerHostPresentation::default();

    presentation.select_container(first.clone());
    presentation.set_container_tab(HostContainerTab::Stats);
    presentation.select_container(first.clone());
    assert_eq!(
        presentation.page(),
        &ContainerHostPage::Container { target: first }
    );

    presentation.select_container_for_inspection(second.clone(), None);
    assert_eq!(
        presentation.page(),
        &ContainerHostPage::Container { target: second }
    );
    assert_eq!(presentation.container_tab(), HostContainerTab::Stats);
}

/// 从容器列表进入 inspector 默认显示 Details，Stats 由 inspector 页签切换。
///
/// @author kongweiguang
#[test]
fn container_inspection_starts_on_details_and_switches_to_stats_in_place() {
    let target = ContainerTarget {
        host_id: Some(HostId::new()),
        runtime: ContainerRuntime::Docker,
        container_id: "container-a".to_owned(),
    };
    let mut presentation = ContainerHostPresentation::default();

    presentation.set_container_tab(HostContainerTab::Stats);
    presentation.select_container_for_inspection(target.clone(), Some(HostContainerTab::Details));
    assert_eq!(
        presentation.page(),
        &ContainerHostPage::Container {
            target: target.clone()
        }
    );
    assert_eq!(presentation.container_tab(), HostContainerTab::Details);

    presentation.set_container_tab(HostContainerTab::Stats);
    assert_eq!(presentation.container_tab(), HostContainerTab::Stats);
}

/// 日志、固定和生命周期等行菜单动作只更新选择，不替换当前列表页。
///
/// @author kongweiguang
#[test]
fn row_action_selection_does_not_leave_the_current_list_page() {
    let target = ContainerTarget {
        host_id: Some(HostId::new()),
        runtime: ContainerRuntime::Docker,
        container_id: "container-a".to_owned(),
    };
    let mut presentation = ContainerHostPresentation::default();

    presentation.select_container_row(target.clone());

    assert_eq!(presentation.page(), &ContainerHostPage::List);
    assert!(presentation.is_container_selected(&target));
}

/// 宿主或 runtime 改变只清详情与展开组，筛选偏好按 main 的 useState 生命周期保留。
///
/// @author kongweiguang
#[test]
fn target_reset_clears_navigation_but_preserves_list_preferences() {
    let host_id = HostId::new();
    let first = compose_target(Some(host_id.clone()), "web", "/srv/web");
    let other_host = HostId::new();
    let mut presentation = ContainerHostPresentation::default();

    assert!(presentation.reset_for_target(Some(&host_id), ContainerRuntime::Docker));
    assert!(!presentation.reset_for_target(Some(&host_id), ContainerRuntime::Docker));
    presentation.set_include_stopped(false);
    presentation.set_group_mode(ContainerHostGroupMode::Status);
    presentation.toggle_project_expanded(first.clone());
    presentation.select_project(first.clone());
    presentation.set_project_tab(ComposeProjectTab::Yaml);
    presentation.select_container(ContainerTarget {
        host_id: Some(host_id.clone()),
        runtime: ContainerRuntime::Docker,
        container_id: "container-a".to_owned(),
    });
    presentation.set_container_tab(HostContainerTab::Stats);

    assert!(presentation.reset_for_target(Some(&host_id), ContainerRuntime::Podman));
    assert_eq!(presentation.page(), &ContainerHostPage::List);
    assert!(!presentation.is_project_expanded(&first));
    assert!(!presentation.include_stopped());
    assert_eq!(presentation.group_mode(), ContainerHostGroupMode::Status);
    assert_eq!(presentation.project_tab(), ComposeProjectTab::Overview);
    assert_eq!(presentation.container_tab(), HostContainerTab::Details);

    presentation.select_project(first.clone());
    presentation.toggle_project_expanded(first.clone());
    assert!(presentation.reset_for_target(Some(&other_host), ContainerRuntime::Podman));
    assert_eq!(presentation.page(), &ContainerHostPage::List);
    assert!(!presentation.is_project_expanded(&first));
    assert!(!presentation.include_stopped());
    assert_eq!(presentation.group_mode(), ContainerHostGroupMode::Status);
}

/// 分组展开通过完整 target 区分同名项目，并支持可逆折叠。
///
/// @author kongweiguang
#[test]
fn expanded_projects_use_complete_target_identity() {
    let host_id = Some(HostId::new());
    let first = compose_target(host_id.clone(), "web", "/srv/one");
    let second = compose_target(host_id, "web", "/srv/two");
    let mut presentation = ContainerHostPresentation::default();

    presentation.toggle_project_expanded(first.clone());
    assert!(presentation.is_project_expanded(&first));
    assert!(!presentation.is_project_expanded(&second));
    presentation.toggle_project_expanded(first.clone());
    assert!(!presentation.is_project_expanded(&first));
}
