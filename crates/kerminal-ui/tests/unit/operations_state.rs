//! 运维独立区块加载生命周期与过期回执测试。
//!
//! @author kongweiguang

use super::{
    ContainerStatusFilter, DirtyOperationTargets, LatestRequestState, SectionPhase,
    SectionSelection, SectionState, ServerInfoHistory, ServerNetworkFilter,
    TMUX_DEFERRED_FEEDBACK_CAPACITY, TmuxKilledDetailGuard, TmuxMutationFeedback,
    TmuxMutationIdentity, defer_tmux_feedback, filter_containers, filtered_network_rates,
    group_container_projects, group_container_projects_with_retained, network_rate_label,
    take_deferred_tmux_feedback, time_series_position, tmux_confirmation_session_matches,
};
use crate::features::operations::{
    ActiveOperationsPaneTarget, ContainerOutputKind, ContainerOutputTarget, DetailReadPhase,
    DetailReadState, OperationsSection, OperationsView, TmuxAttachTarget,
    TmuxAttachTerminalBinding, TmuxDetachIntent, TmuxDetailState, TmuxSnapshot,
    ToolRailOperationTarget, forward_form_can_save, forward_unavailable_hint,
    plan_tmux_attach_transition, resolve_forward_add_host, resolve_forward_form_host,
    resolve_ports_panel_target, resolve_tool_rail_operation_target,
    tmux_attach_binding_matches_target, tmux_detach_intent_can_send,
    tmux_detach_intent_matches_current,
};
use kerminal_application::operations::TmuxCapability;
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    files::ContainerRuntime,
    identity::{HostId, PaneId, SessionId, TabId},
    operations::{
        ComposeMetadata, ComposeProjectTarget, ContainerRuntime as OperationsContainerRuntime,
        ContainerStatus, ContainerSummary, ContainerTarget, CpuSnapshot, DiskSnapshot, GpuSnapshot,
        MemorySnapshot, NetworkCounterSnapshot, NetworkRateSnapshot, ServerInfoProbeFailure,
        ServerInfoProbeStatus, ServerInfoSampleTimes, ServerInfoSnapshot, ServerInfoTarget,
        TmuxSession, TmuxTarget,
    },
    sessions::SessionOwner,
};
use std::collections::VecDeque;

/// 刷新可更新活动信息，但不能把退出、重建、改名或其它 socket 的会话当作旧确认对象。
/// @author kongweiguang
#[test]
fn tmux_confirmation_rejects_replaced_sessions_but_keeps_activity_updates() {
    let expected = TmuxSession {
        id: "$1".into(),
        name: "work".into(),
        attached: false,
        clients: 0,
        windows: 1,
        current_path: None,
        created_at: Some(100),
        activity_at: Some(100),
        target_ref: "ssh-fixture:default".into(),
    };
    let activity_update = TmuxSession {
        attached: true,
        clients: 2,
        windows: 3,
        current_path: Some("/tmp".into()),
        activity_at: Some(200),
        ..expected.clone()
    };
    assert!(tmux_confirmation_session_matches(
        &expected,
        Some(&activity_update)
    ));
    assert!(!tmux_confirmation_session_matches(&expected, None));
    for replacement in [
        TmuxSession {
            id: "$2".into(),
            ..expected.clone()
        },
        TmuxSession {
            name: "renamed".into(),
            ..expected.clone()
        },
        TmuxSession {
            created_at: Some(101),
            ..expected.clone()
        },
        TmuxSession {
            created_at: None,
            ..expected.clone()
        },
        TmuxSession {
            target_ref: "other-socket".into(),
            ..expected.clone()
        },
    ] {
        assert!(!tmux_confirmation_session_matches(
            &expected,
            Some(&replacement)
        ));
    }
}

/// kill 成功但换 pane 抑制状态回执时，只有冻结的目标/代次和稳定会话身份仍一致才清理旧详情。
/// 该测试验证生产清理门禁；完整异步回执由 operations_tmux_binding 的内存命令端口覆盖。
///
/// @author kongweiguang
#[test]
fn confirmed_kill_detail_cleanup_requires_frozen_identity_and_success() {
    let target = TmuxTarget {
        host_id: Some(HostId::new()),
        socket_name: Some("work".to_owned()),
        ..TmuxTarget::default()
    };
    let details = TmuxDetailState {
        session_id: Some("$1".to_owned()),
        session_name: Some("work".to_owned()),
        windows: Some(Vec::new()),
        generation: 7,
        ..TmuxDetailState::default()
    };
    let session = TmuxSession {
        id: "$1".into(),
        name: "work".into(),
        attached: false,
        clients: 0,
        windows: 1,
        current_path: None,
        created_at: Some(100),
        activity_at: Some(100),
        target_ref: "ssh-fixture:work".into(),
    };
    let guard = TmuxKilledDetailGuard::freeze(&target, 5, &details, Some(&session))
        .expect("complete identity should freeze cleanup guard");
    let listed = TmuxSession {
        attached: true,
        clients: 2,
        activity_at: Some(200),
        ..session.clone()
    };

    // 列表刷新只更新 attached/clients 等描述，不重载窗口/pane 详情，也不推进详情代次；
    // 即使 tmux server 在同一秒内重启并用尽身份字段，此处清理的仍只可能是提交时的旧详情。
    // 新会话的详情只有经重新选择或窗口/分屏重新载入才会出现，而那些路径都会递增详情代次
    // 并被清理门禁挡下；单独的 capture 读取不递增代次，但清理后旧回执仍会被拒绝。
    assert!(guard.can_clear(true, &target, 5, &details, std::slice::from_ref(&listed)));
    assert!(!guard.can_clear(false, &target, 5, &details, std::slice::from_ref(&listed)));

    // 切到其它 profile 或目标代次前进后，旧详情已不属于当前查询。
    let other_target = TmuxTarget {
        profile_id: Some("beta".to_owned()),
        ..target.clone()
    };
    assert!(!guard.can_clear(
        true,
        &other_target,
        5,
        &details,
        std::slice::from_ref(&listed)
    ));
    assert!(!guard.can_clear(true, &target, 6, &details, std::slice::from_ref(&listed)));

    // 用户快速重选任意会话都会递增详情代次；即使仍选同一 ID，也必须保守保留。
    let reselected_same_id = TmuxDetailState {
        session_id: Some("$1".to_owned()),
        generation: 8,
        ..TmuxDetailState::default()
    };
    assert!(!guard.can_clear(
        true,
        &target,
        5,
        &reselected_same_id,
        std::slice::from_ref(&listed)
    ));
    let selected_other = TmuxDetailState {
        session_id: Some("$2".to_owned()),
        generation: 7,
        ..TmuxDetailState::default()
    };
    assert!(!guard.can_clear(
        true,
        &target,
        5,
        &selected_other,
        std::slice::from_ref(&listed)
    ));

    // 列表已无该会话，或同 ID 会话已重建时都不能再清理。
    assert!(!guard.can_clear(true, &target, 5, &details, &[]));
    let rebuilt = TmuxSession {
        created_at: Some(101),
        ..session.clone()
    };
    assert!(!guard.can_clear(true, &target, 5, &details, &[rebuilt]));

    // 缺少创建时间时无法跨刷新证明身份，冻结阶段就放弃清理。
    let unknown_identity = TmuxSession {
        created_at: None,
        ..session
    };
    assert!(TmuxKilledDetailGuard::freeze(&target, 5, &details, Some(&unknown_identity)).is_none());
    assert!(
        TmuxKilledDetailGuard::freeze(&target, 5, &selected_other, Some(&unknown_identity))
            .is_none()
    );
}

/// 只有真实 `list_sessions` 成功（含空列表）才清除写后 dirty；probe-only 不可用快照保留核对机会。
/// 该测试验证 `load_tmux` 的生产判定谓词和 dirty 转换，不冒充完整异步读取测试。
///
/// @author kongweiguang
#[test]
fn tmux_dirty_is_consumed_only_by_an_actual_session_list() {
    let target = TmuxTarget {
        host_id: Some(HostId::new()),
        ..TmuxTarget::default()
    };
    let snapshot = |available: bool| TmuxSnapshot {
        capability: TmuxCapability {
            target_ref: "ssh-fixture:work".to_owned(),
            available,
            version: available.then(|| "tmux 3.4".to_owned()),
            reason: (!available).then(|| "tmux unavailable".to_owned()),
        },
        sessions: Vec::new(),
    };

    let probe_only = snapshot(false);
    assert!(!OperationsView::tmux_snapshot_confirms_refresh(&probe_only));
    let empty_list = snapshot(true);
    assert!(OperationsView::tmux_snapshot_confirms_refresh(&empty_list));

    let mut dirty = DirtyOperationTargets::default();
    dirty.mark_tmux_dirty(target.clone());
    if OperationsView::tmux_snapshot_confirms_refresh(&probe_only) {
        dirty.tmux_refreshed(&target);
    }
    assert!(dirty.tmux_is_dirty(&target));
    if OperationsView::tmux_snapshot_confirms_refresh(&empty_list) {
        dirty.tmux_refreshed(&target);
    }
    assert!(!dirty.tmux_is_dirty(&target));
}

/// Compose 拆除后没有容器行仍保留完整目标，不会与同名但不同目录的项目合并。
#[test]
fn retained_compose_project_stays_visible_without_live_containers() {
    let first = ComposeProjectTarget {
        host_id: None,
        runtime: OperationsContainerRuntime::Docker,
        project: "fixture".to_owned(),
        working_dir: "/srv/one".to_owned(),
        config_files: vec!["compose.yml".to_owned()],
    };
    let mut second = first.clone();
    second.working_dir = "/srv/two".to_owned();
    let groups = group_container_projects_with_retained(
        Vec::new(),
        &[first.clone(), second.clone(), first.clone()],
    );
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].target, Some(first));
    assert_eq!(groups[1].target, Some(second));
    assert!(
        groups
            .iter()
            .all(|group| group.containers.is_empty() && group.compose_group)
    );
}

/// ToolRail 面板固定自身分区，另一个面板的导航变化不能改写其活动内容。
#[test]
fn fixed_panel_sections_are_independent_from_each_other_and_dashboard() {
    let mut tmux_panel = SectionSelection::fixed(OperationsSection::Tmux);
    let ports_panel = SectionSelection::fixed(OperationsSection::PortForwards);
    let mut dashboard = SectionSelection::dashboard();

    assert!(!tmux_panel.select(OperationsSection::Containers));
    assert_eq!(tmux_panel.active(), OperationsSection::Tmux);
    assert_eq!(tmux_panel.refresh_scope(), Some(OperationsSection::Tmux));
    assert!(!tmux_panel.watches_forwards());
    assert_eq!(ports_panel.active(), OperationsSection::PortForwards);
    assert_eq!(
        ports_panel.refresh_scope(),
        Some(OperationsSection::PortForwards)
    );
    assert!(ports_panel.watches_forwards());

    assert!(dashboard.select(OperationsSection::Containers));
    assert_eq!(dashboard.active(), OperationsSection::Containers);
    assert_eq!(dashboard.refresh_scope(), None);
    assert!(dashboard.watches_forwards());
    assert_eq!(tmux_panel.active(), OperationsSection::Tmux);
    assert_eq!(ports_panel.active(), OperationsSection::PortForwards);
}

/// 失败不能伪装为空列表，且重试失败时仍能保留并标注旧的成功快照。
#[test]
fn section_failure_preserves_last_success_without_claiming_ready() {
    let mut section = SectionState::new();
    assert_eq!(section.phase(), SectionPhase::NotLoaded);
    assert!(section.data().is_none());

    let initial = section.begin(1);
    assert!(section.complete(&initial, Ok(vec!["container-1"])));
    assert_eq!(section.phase(), SectionPhase::Ready);
    assert_eq!(section.data(), Some(&vec!["container-1"]));

    let retry = section.begin(1);
    assert_eq!(section.phase(), SectionPhase::Loading);
    assert!(section.complete(&retry, Err("runtime unavailable".to_owned())));
    assert_eq!(section.phase(), SectionPhase::Failed);
    assert_eq!(section.error(), Some("runtime unavailable"));
    assert_eq!(section.data(), Some(&vec!["container-1"]));
}

/// 同一 section 的旧请求即使晚于新请求返回，也不能覆盖最新成功快照。
#[test]
fn older_request_result_is_ignored_after_a_retry() {
    let mut section = SectionState::new();
    let old = section.begin(4);
    let latest = section.begin(4);

    assert!(!section.complete(&old, Ok("stale")));
    assert_eq!(section.phase(), SectionPhase::Loading);
    assert!(section.complete(&latest, Ok("fresh")));
    assert_eq!(section.data(), Some(&"fresh"));
}

/// 新的详情查询会取消旧请求，只允许最后一次选择修改共享输出面板。
#[test]
fn latest_request_cancels_older_ticket_and_accepts_only_current_receipt() {
    let mut requests = LatestRequestState::default();
    let old = requests.begin();
    let old_cancellation = old.cancellation.clone();
    let latest = requests.begin();

    assert!(old_cancellation.is_cancelled());
    assert!(!requests.is_current(&old));
    assert!(requests.is_current(&latest));
    assert!(requests.complete(&latest));
    assert!(!requests.is_current(&latest));
}

/// 执行器取消当前查询后区块退出 loading，并保留已成功读取的数据。
#[test]
fn cancelling_current_section_request_preserves_snapshot_and_leaves_loading() {
    let mut section = SectionState::new();
    let loaded = section.begin(1);
    assert!(section.complete(&loaded, Ok(vec!["snapshot"])));
    let pending = section.begin(1);

    assert!(section.cancel(&pending));
    assert_eq!(section.phase(), SectionPhase::Cancelled);
    assert_eq!(section.data(), Some(&vec!["snapshot"]));
    assert!(!section.is_current(&pending));
}

/// 目标切换会取消未完成请求、清空旧目标快照，并拒绝旧目标的迟到回执。
#[test]
fn target_reset_cancels_pending_request_and_rejects_late_result() {
    let mut section = SectionState::new();
    let old = section.begin(11);
    let cancellation = old.cancellation.clone();

    section.reset(12);

    assert!(cancellation.is_cancelled());
    assert_eq!(section.phase(), SectionPhase::NotLoaded);
    assert!(section.data().is_none());
    assert!(!section.complete(&old, Ok("old target")));
}

/// 只有无 pane 时借用选择主机；Local、SSH 和不可用 pane 都保持自己的 target 语义。
///
/// @author kongweiguang
#[test]
fn selected_host_fallback_requires_absent_pane_context() {
    let selected = HostId::new();
    let active_host = HostId::new();
    assert_eq!(
        resolve_tool_rail_operation_target(
            ActiveOperationsPaneTarget::Absent,
            Some(selected.clone()),
        ),
        ToolRailOperationTarget::Ssh { host_id: selected },
    );
    assert_eq!(
        resolve_tool_rail_operation_target(
            ActiveOperationsPaneTarget::Local,
            Some(active_host.clone()),
        ),
        ToolRailOperationTarget::Local,
    );
    assert_eq!(
        resolve_tool_rail_operation_target(
            ActiveOperationsPaneTarget::Ssh(active_host.clone()),
            Some(HostId::new()),
        ),
        ToolRailOperationTarget::Ssh {
            host_id: active_host
        },
    );
    assert_eq!(
        resolve_tool_rail_operation_target(
            ActiveOperationsPaneTarget::Unavailable,
            Some(HostId::new()),
        ),
        ToolRailOperationTarget::Unavailable,
    );
    assert_eq!(
        resolve_tool_rail_operation_target(ActiveOperationsPaneTarget::Absent, None),
        ToolRailOperationTarget::Unavailable,
    );
}

/// Add 只在无固定工具目标的完整页使用所选 SSH；固定 Local/Unavailable 不借主机回退。
///
/// @author kongweiguang
#[test]
fn forward_add_target_requires_an_explicit_ssh_target() {
    let selected = HostId::parse("selected-host").unwrap();
    let fixed = HostId::parse("fixed-host").unwrap();
    let fixed_ssh = ToolRailOperationTarget::Ssh {
        host_id: fixed.clone(),
    };

    assert_eq!(
        resolve_forward_add_host(None, Some(&selected)),
        Some(selected.clone())
    );
    assert_eq!(
        resolve_forward_add_host(Some(&fixed_ssh), Some(&selected)),
        Some(fixed)
    );
    assert_eq!(
        resolve_forward_add_host(Some(&ToolRailOperationTarget::Local), Some(&selected)),
        None
    );
    assert_eq!(
        resolve_forward_add_host(Some(&ToolRailOperationTarget::Unavailable), Some(&selected)),
        None
    );
}

/// 容器 pane 即使带有 SSH 宿主也只显示专用说明，不能借侧栏选择主机创建隧道。
///
/// @author kongweiguang
#[test]
fn container_forwarding_context_shows_hint_without_host_fallback() {
    let container_host = HostId::parse("container-host").unwrap();
    let selected_host = HostId::parse("selected-host").unwrap();
    let container_target = ContainerTarget {
        host_id: Some(container_host.clone()),
        runtime: OperationsContainerRuntime::Docker,
        container_id: "container-1".to_owned(),
    };
    let target = resolve_ports_panel_target(
        ToolRailOperationTarget::Ssh {
            host_id: container_host,
        },
        Some(&container_target),
    );

    assert_eq!(target, ToolRailOperationTarget::Unavailable);
    assert_eq!(
        forward_unavailable_hint(Some(&container_target)).0,
        "当前容器目标不支持直接创建 SSH 隧道。请切回宿主 SSH 主机管理端口转发。"
    );
    assert_eq!(
        resolve_forward_add_host(Some(&target), Some(&selected_host)),
        None
    );
}

/// Add 保存持续使用打开时冻结的 HostId，编辑优先绑定原定义且无快照时保持无目标。
///
/// @author kongweiguang
#[test]
fn forward_form_uses_frozen_add_host_and_original_edit_host() {
    let frozen = HostId::parse("host-a").unwrap();
    let selected_after_open = HostId::parse("host-b").unwrap();

    let add_target = resolve_forward_form_host(None, Some(&frozen));
    assert_eq!(add_target, Some(frozen.clone()));
    assert_ne!(add_target.as_ref(), Some(&selected_after_open));
    assert_eq!(
        resolve_forward_form_host(Some(&selected_after_open), Some(&frozen)),
        Some(selected_after_open)
    );
    assert_eq!(resolve_forward_form_host(None, None), None);
}

/// 没有冻结主机或表单已关闭时禁用提交，不会用新选中目标补齐缺失快照。
///
/// @author kongweiguang
#[test]
fn forward_form_save_requires_open_form_and_frozen_host() {
    let host = HostId::parse("host-a").unwrap();

    assert!(forward_form_can_save(true, Some(&host)));
    assert!(!forward_form_can_save(true, None));
    assert!(!forward_form_can_save(false, Some(&host)));
}

/// 同一代次策略拒绝所选主机切换前发出的迟到 section 回执。
///
/// @author kongweiguang
#[test]
fn selected_host_switch_rejects_late_server_result() {
    let mut server = SectionState::new();
    let host_a_request = server.begin(41);
    let cancellation = host_a_request.cancellation.clone();
    server.reset(42);

    assert!(cancellation.is_cancelled());
    assert!(!server.complete(&host_a_request, Ok("host A snapshot")));
    let host_b_request = server.begin(42);
    assert!(server.complete(&host_b_request, Ok("host B snapshot")));
    assert_eq!(server.data(), Some(&"host B snapshot"));
}

/// 所选 SSH 主机只允许 tmux 查询；attach 要求存在且匹配精确的 pane target binding。
///
/// @author kongweiguang
#[test]
fn selected_host_tmux_query_does_not_enable_unbound_attach() {
    let host_a = HostId::new();
    let query_target = kerminal_domain::operations::TmuxTarget {
        host_id: Some(host_a.clone()),
        ..kerminal_domain::operations::TmuxTarget::default()
    };
    let matching = TmuxAttachTarget::Ssh { host_id: host_a };
    let mismatching = TmuxAttachTarget::Ssh {
        host_id: HostId::new(),
    };

    assert!(!tmux_attach_binding_matches_target(None, &query_target));
    assert!(!tmux_attach_binding_matches_target(
        Some(&mismatching),
        &query_target,
    ));
    assert!(tmux_attach_binding_matches_target(
        Some(&matching),
        &query_target,
    ));
}

/// 构造可区分 pane owner 与 tmux host/profile 身份的固定栏绑定，供切换边界回归使用。
///
/// TabId 由调用方提供，使同一场景的 A/B 只改变 pane、session 与 owner generation，
/// 避免把“换 Tab”误当成“同 Tab 换 pane”而放松 owner 断言。
///
/// @author kongweiguang
fn tmux_attach_binding_fixture(
    tab_id: TabId,
    host_id: Option<HostId>,
    profile_id: Option<&str>,
    pane_id: PaneId,
) -> TmuxAttachTerminalBinding {
    TmuxAttachTerminalBinding {
        pane_id: pane_id.clone(),
        target: host_id.map_or_else(
            || TmuxAttachTarget::Local {
                profile_id: profile_id.map(str::to_owned),
            },
            |host_id| TmuxAttachTarget::Ssh { host_id },
        ),
        binding: UiSessionBinding {
            session_id: SessionId::new(),
            owner: SessionOwner::Ui { tab_id, pane_id },
            session_generation: 4,
        },
        working_directory: None,
    }
}

/// 给纯身份比较测试构造真实 UI owner，使 pane 与重连 generation 都参与完全相等校验。
///
/// @author kongweiguang
fn tmux_detach_intent_fixture() -> (TmuxDetachIntent, TmuxTarget, TmuxAttachTerminalBinding) {
    let host_id = HostId::new();
    let pane_id = PaneId::new();
    let terminal = TmuxAttachTerminalBinding {
        pane_id: pane_id.clone(),
        target: TmuxAttachTarget::Ssh {
            host_id: host_id.clone(),
        },
        binding: UiSessionBinding {
            session_id: SessionId::new(),
            owner: SessionOwner::Ui {
                tab_id: TabId::new(),
                pane_id,
            },
            session_generation: 4,
        },
        working_directory: Some("/srv/app".to_owned()),
    };
    let target = TmuxTarget {
        host_id: Some(host_id),
        socket_name: Some("work".to_owned()),
        ..TmuxTarget::default()
    };
    let intent = TmuxDetachIntent {
        target: target.clone(),
        terminal: terminal.clone(),
    };
    (intent, target, terminal)
}

/// Header detach 不依赖先前 attach 或 session 快照，但仍绑定 tmux target 与 pane owner/generation。
///
/// @author kongweiguang
#[test]
fn tmux_detach_intent_rejects_stale_target_and_terminal_binding() {
    let (intent, target, terminal) = tmux_detach_intent_fixture();
    assert!(tmux_detach_intent_matches_current(
        &intent, &target, &terminal
    ));
    assert!(tmux_detach_intent_can_send(
        &intent, &target, &terminal, false, false
    ));
    assert!(!tmux_detach_intent_can_send(
        &intent, &target, &terminal, true, false
    ));
    assert!(!tmux_detach_intent_can_send(
        &intent, &target, &terminal, false, true
    ));

    let other_socket = TmuxTarget {
        socket_name: Some("other".to_owned()),
        ..target.clone()
    };
    assert!(!tmux_detach_intent_matches_current(
        &intent,
        &other_socket,
        &terminal
    ));

    let mut reconnected_terminal = terminal.clone();
    reconnected_terminal.binding.session_generation += 1;
    assert!(!tmux_detach_intent_matches_current(
        &intent,
        &target,
        &reconnected_terminal,
    ));

    let mut changed_working_directory = terminal.clone();
    changed_working_directory.working_directory = Some("/srv/other".to_owned());
    assert!(tmux_detach_intent_matches_current(
        &intent,
        &target,
        &changed_working_directory,
    ));

    let mut other_pane = terminal.clone();
    other_pane.pane_id = PaneId::new();
    assert!(!tmux_detach_intent_matches_current(
        &intent,
        &target,
        &other_pane
    ));
}

/// 同 host 不同 pane 只更换 attach owner，tmux 查询目标和已加载数据仍可复用。
///
/// @author kongweiguang
#[test]
fn tmux_attach_owner_switch_on_same_ssh_host_keeps_query_data() {
    let tab_id = TabId::new();
    let host_id = HostId::new();
    let previous =
        tmux_attach_binding_fixture(tab_id.clone(), Some(host_id.clone()), None, PaneId::new());
    let next = tmux_attach_binding_fixture(tab_id, Some(host_id), None, PaneId::new());
    let previous_target = previous.target.to_tmux_target();
    let next_target = next.target.to_tmux_target();
    assert_eq!(previous_target, next_target);

    let transition = plan_tmux_attach_transition(
        Some(&previous),
        Some(&next),
        &previous_target,
        &next_target,
        true,
        SectionPhase::Ready,
        true,
    );
    assert!(transition.owner_changed);
    // owner_changed 在 setter 中驱动视图代次递增、冻结确认清理和 detach 撤销；
    // 这里要求 reset_reads/load_sessions 为假，固定 setter 应采用的复用决策；
    // 列表和详情的实际保留由 operations_tmux_binding 的真实 setter 回归覆盖。
    assert!(!transition.reset_reads);
    assert!(!transition.load_sessions);

    // SSH 查询目标仍有效且刚刚获得 attach binding 时，不应丢弃宿主级列表或重读。
    let transition = plan_tmux_attach_transition(
        None,
        Some(&next),
        &next_target,
        &next_target,
        true,
        SectionPhase::Ready,
        true,
    );
    assert!(transition.owner_changed);
    assert!(!transition.reset_reads);
    assert!(!transition.load_sessions);

    // 工作目录只影响 attach 输入，不是 tmux 查询身份；同一 owner 的 cwd 变化不应触发重读。
    let mut changed_working_directory = next.clone();
    changed_working_directory.working_directory = Some("/srv/other".to_owned());
    let transition = plan_tmux_attach_transition(
        Some(&next),
        Some(&changed_working_directory),
        &next_target,
        &next_target,
        true,
        SectionPhase::Ready,
        true,
    );
    assert!(!transition.owner_changed);
    assert!(!transition.reset_reads);
    assert!(!transition.load_sessions);
}

/// host/profile 身份变化才重置 tmux 列表与详情，并在可见时读取新目标。
///
/// @author kongweiguang
#[test]
fn tmux_attach_target_change_resets_and_reads_current_target() {
    let tab_id = TabId::new();
    let previous =
        tmux_attach_binding_fixture(tab_id.clone(), Some(HostId::new()), None, PaneId::new());
    let next = tmux_attach_binding_fixture(tab_id, Some(HostId::new()), None, PaneId::new());
    let previous_target = previous.target.to_tmux_target();
    let next_target = next.target.to_tmux_target();
    assert_ne!(previous_target, next_target);

    let transition = plan_tmux_attach_transition(
        Some(&previous),
        Some(&next),
        &previous_target,
        &next_target,
        true,
        SectionPhase::Ready,
        true,
    );
    assert!(transition.owner_changed);
    assert!(transition.reset_reads);
    assert!(transition.load_sessions);

    let tab_id = TabId::new();
    let previous = tmux_attach_binding_fixture(tab_id.clone(), None, Some("alpha"), PaneId::new());
    let next = tmux_attach_binding_fixture(tab_id, None, Some("beta"), PaneId::new());
    let previous_target = previous.target.to_tmux_target();
    let next_target = next.target.to_tmux_target();
    assert_ne!(previous_target, next_target);
    let transition = plan_tmux_attach_transition(
        Some(&previous),
        Some(&next),
        &previous_target,
        &next_target,
        true,
        SectionPhase::Ready,
        true,
    );
    assert!(transition.owner_changed);
    assert!(transition.reset_reads);
    assert!(transition.load_sessions);
}

/// 目标失效时先丢弃旧查询和详情；不能为复用列表继续展示失效目标的数据。
///
/// @author kongweiguang
#[test]
fn tmux_attach_invalid_target_resets_without_refetch() {
    let previous =
        tmux_attach_binding_fixture(TabId::new(), Some(HostId::new()), None, PaneId::new());
    let target = previous.target.to_tmux_target();
    let transition = plan_tmux_attach_transition(
        Some(&previous),
        None,
        &target,
        &target,
        false,
        SectionPhase::Ready,
        true,
    );
    assert!(transition.owner_changed);
    assert!(transition.reset_reads);
    assert!(!transition.load_sessions);
}

/// 面板隐藏或尚未初始化时只登记重置；同目标首次可见时仍允许补读。
///
/// @author kongweiguang
#[test]
fn tmux_attach_target_change_waits_for_activation_and_initial_load() {
    let tab_id = TabId::new();
    let host_id = HostId::new();
    let previous =
        tmux_attach_binding_fixture(tab_id.clone(), Some(host_id.clone()), None, PaneId::new());
    let next = tmux_attach_binding_fixture(tab_id.clone(), Some(host_id), None, PaneId::new());
    let previous_target = previous.target.to_tmux_target();
    let next_target = next.target.to_tmux_target();
    assert_eq!(previous_target, next_target);

    let other_previous =
        tmux_attach_binding_fixture(tab_id.clone(), Some(HostId::new()), None, PaneId::new());
    let other_next = tmux_attach_binding_fixture(tab_id, Some(HostId::new()), None, PaneId::new());
    let other_previous_target = other_previous.target.to_tmux_target();
    let other_next_target = other_next.target.to_tmux_target();
    assert_ne!(other_previous_target, other_next_target);

    let hidden = plan_tmux_attach_transition(
        Some(&other_previous),
        Some(&other_next),
        &other_previous_target,
        &other_next_target,
        true,
        SectionPhase::Ready,
        false,
    );
    assert!(hidden.owner_changed);
    assert!(hidden.reset_reads);
    assert!(!hidden.load_sessions);

    let same_target_not_loaded = plan_tmux_attach_transition(
        Some(&previous),
        Some(&next),
        &previous_target,
        &next_target,
        true,
        SectionPhase::NotLoaded,
        true,
    );
    assert!(same_target_not_loaded.owner_changed);
    assert!(!same_target_not_loaded.reset_reads);
    assert!(same_target_not_loaded.load_sessions);
}

/// 同目标正在读取、成功、失败或取消时换 pane 不重复请求，保留快照供用户操作。
///
/// @author kongweiguang
#[test]
fn tmux_attach_same_target_keeps_inflight_and_retryable_reads() {
    let tab_id = TabId::new();
    let host_id = HostId::new();
    let previous =
        tmux_attach_binding_fixture(tab_id.clone(), Some(host_id.clone()), None, PaneId::new());
    let next = tmux_attach_binding_fixture(tab_id, Some(host_id), None, PaneId::new());
    let target = previous.target.to_tmux_target();
    assert_eq!(target, next.target.to_tmux_target());

    // Failed/Cancelled 也保持现状：切 pane 不自动重试，避免在用户未确认时重复远端读取；
    // 需要恢复时由用户显式刷新触发重新读取。
    for phase in [
        SectionPhase::Loading,
        SectionPhase::Ready,
        SectionPhase::Failed,
        SectionPhase::Cancelled,
    ] {
        let transition = plan_tmux_attach_transition(
            Some(&previous),
            Some(&next),
            &target,
            &target,
            true,
            phase,
            true,
        );
        assert!(transition.owner_changed);
        assert!(!transition.reset_reads, "phase {phase:?}");
        assert!(!transition.load_sessions, "phase {phase:?}");
    }
}

/// 面板被关闭时 section 会撤销尚未结束的读取任务。
#[test]
fn dropping_section_cancels_pending_request() {
    let mut section = SectionState::<()>::new();
    let ticket = section.begin(1);
    let cancellation = ticket.cancellation.clone();

    drop(section);

    assert!(cancellation.is_cancelled());
}

/// 并排 ToolRail 实例各持自己的刷新票据，关闭一个面板不取消另一个面板的读取。
#[test]
fn panel_refresh_cancellation_is_scoped_to_its_own_section_state() {
    let mut left_panel = SectionState::<u8>::new();
    let mut bottom_panel = SectionState::<u8>::new();
    let left_ticket = left_panel.begin(1);
    let bottom_ticket = bottom_panel.begin(1);

    left_panel.reset(2);

    assert!(left_ticket.cancellation.is_cancelled());
    assert!(!bottom_ticket.cancellation.is_cancelled());
    assert!(bottom_panel.complete(&bottom_ticket, Ok(7)));
    assert_eq!(bottom_panel.data(), Some(&7));
}

/// 旧目标的迟到容器回执只标记原 host/runtime；切回后的成功权威读取才清脏。
#[test]
fn stale_container_mutation_marks_original_target_until_authoritative_refresh() {
    let old_host = Some(HostId::new());
    let current_host = Some(HostId::new());
    let mut dirty = DirtyOperationTargets::default();

    assert!(!dirty.container_mutation_completed(
        old_host.clone(),
        ContainerRuntime::Docker,
        &current_host,
        ContainerRuntime::Docker,
    ));
    assert!(
        dirty
            .containers
            .contains(&(old_host.clone(), ContainerRuntime::Docker))
    );
    assert!(
        !dirty
            .containers
            .contains(&(current_host.clone(), ContainerRuntime::Docker))
    );

    assert!(dirty.container_mutation_completed(
        old_host.clone(),
        ContainerRuntime::Docker,
        &old_host,
        ContainerRuntime::Docker,
    ));
    assert!(
        dirty
            .containers
            .contains(&(old_host.clone(), ContainerRuntime::Docker))
    );
    dirty.container_refreshed(&old_host, ContainerRuntime::Docker);
    assert!(
        !dirty
            .containers
            .contains(&(old_host.clone(), ContainerRuntime::Docker))
    );
}

/// tmux 回执按完整查询目标刷新；dirty 只保留当前目标的未确认状态，由目标重置重新置位。
///
/// 同目标 pane 变化只阻止状态改写；旧目标迟到写回不缓存，切回时依赖重置先产生 dirty。
///
/// @author kongweiguang
#[test]
fn tmux_mutation_decision_scopes_refresh_and_dirty_to_full_target() {
    let host = HostId::new();
    let ssh_target = TmuxTarget {
        host_id: Some(host.clone()),
        ..TmuxTarget::default()
    };
    let mut dirty = DirtyOperationTargets::default();

    let same_view = dirty.tmux_mutation_completed(ssh_target.clone(), &ssh_target, 3, 3);
    assert!(same_view.refresh_current_target);
    assert!(same_view.update_current_view);
    assert!(dirty.tmux_is_dirty(&ssh_target));
    dirty.tmux_refreshed(&ssh_target);
    assert!(!dirty.tmux_is_dirty(&ssh_target));

    // 同 query 目标只换 pane：列表仍可刷新，但旧 pane 的确认/状态不得写回新焦点。
    let changed_pane = dirty.tmux_mutation_completed(ssh_target.clone(), &ssh_target, 3, 4);
    assert!(changed_pane.refresh_current_target);
    assert!(!changed_pane.update_current_view);
    dirty.tmux_refreshed(&ssh_target);

    let alpha = TmuxTarget {
        profile_id: Some("alpha".to_owned()),
        ..TmuxTarget::default()
    };
    let beta = TmuxTarget {
        profile_id: Some("beta".to_owned()),
        ..TmuxTarget::default()
    };
    assert_ne!(alpha, beta);

    // Local profile alpha 的迟到回执到达 beta 时不能触发 beta 刷新或改写 beta 视图；
    // 有界状态不为已离开的 alpha 保留历史 dirty，切回时由目标重置重新置为未确认。
    let late_alpha = dirty.tmux_mutation_completed(alpha.clone(), &beta, 3, 3);
    assert!(!late_alpha.refresh_current_target);
    assert!(!late_alpha.update_current_view);
    assert!(!dirty.tmux_is_dirty(&alpha));
    assert!(!dirty.tmux_is_dirty(&beta));

    // beta 成功读取只清 beta 的 dirty，不能代办 alpha 的未核对变更。
    dirty.tmux_refreshed(&beta);
    assert!(!dirty.tmux_is_dirty(&alpha));
    assert!(!dirty.tmux_is_dirty(&beta));

    // 切回 alpha 属于一次新的目标重置：先标未确认，失败或不可用时保留激活重试机会。
    dirty.mark_tmux_dirty(alpha.clone());
    let returned = dirty.tmux_mutation_completed(alpha.clone(), &alpha, 3, 7);
    assert!(returned.refresh_current_target);
    assert!(!returned.update_current_view);
    // 失败重试不会调用 tmux_refreshed；dirty 保留到同一目标成功读取，避免失败被当成已核对。
    assert!(dirty.tmux_is_dirty(&alpha));
    dirty.tmux_refreshed(&alpha);
    assert!(!dirty.tmux_is_dirty(&alpha));

    // 同 host/profile 但 socket 不同仍是不同查询目标，不能互相刷新或清 dirty。
    let socket_a = TmuxTarget {
        host_id: Some(host.clone()),
        socket_name: Some("a".to_owned()),
        ..TmuxTarget::default()
    };
    let socket_b = TmuxTarget {
        host_id: Some(host),
        socket_name: Some("b".to_owned()),
        ..TmuxTarget::default()
    };
    let other_socket = dirty.tmux_mutation_completed(socket_a.clone(), &socket_b, 1, 1);
    assert!(!other_socket.refresh_current_target);
    assert!(!other_socket.update_current_view);
    assert!(!dirty.tmux_is_dirty(&socket_a));
    assert!(!dirty.tmux_is_dirty(&socket_b));
    dirty.tmux_refreshed(&socket_b);
    assert!(!dirty.tmux_is_dirty(&socket_a));

    // 当前目标重复写回只保留一个未确认目标；切换目标后旧标记被新目标覆盖，状态有固定上界。
    for _ in 0..100 {
        dirty.mark_tmux_dirty(socket_a.clone());
    }
    assert!(dirty.tmux_is_dirty(&socket_a));
    dirty.mark_tmux_dirty(alpha.clone());
    assert!(dirty.tmux_is_dirty(&alpha));
    assert!(!dirty.tmux_is_dirty(&socket_a));
}

/// 在状态模块内核对私有目标，保留迟到成功不能绑定历史或追加样本的回归，避免为测试暴露 getter。
///
/// @author kongweiguang
#[test]
fn stale_server_success_does_not_pollute_history() {
    let mut section = SectionState::new();
    let stale_ticket = section.begin(7);
    let current_ticket = section.begin(7);
    let mut history = ServerInfoHistory::default();
    let stale_snapshot = server_snapshot(ServerInfoTarget::Local, 100);

    assert!(!history.complete_if_current(&mut section, &stale_ticket, stale_snapshot));
    assert!(history.samples().is_empty());
    assert_eq!(history.target, None);
    assert!(section.is_current(&current_ticket));
    assert_eq!(section.phase(), SectionPhase::Loading);
}

/// 监控历史按真实 Fast 时间去重，并在超过有界容量时淘汰最旧点。
#[test]
fn server_history_deduplicates_and_bounds_fast_samples() {
    let mut history = ServerInfoHistory::default();
    for sample_time in 1..=65 {
        assert!(history.record(&server_snapshot(ServerInfoTarget::Local, sample_time)));
    }
    assert!(!history.record(&server_snapshot(ServerInfoTarget::Local, 65)));
    assert_eq!(history.samples().len(), 60);
    assert_eq!(history.samples().front().unwrap().sampled_at_unix_ms, 6);
    assert_eq!(history.samples().back().unwrap().sampled_at_unix_ms, 65);
}

/// 主版先按有效速率排名再筛选，且相同流量保留采集顺序。
///
/// @author kongweiguang
#[test]
fn network_interfaces_rank_sampled_rates_before_filtering() {
    let rates = vec![
        network_rate("lo", Some(0.5), Some(0.5)),
        network_rate("enp0s1", Some(2.0), Some(3.0)),
        network_rate("enp0s2", Some(40.0), Some(10.0)),
        network_rate("enp0s3", Some(20.0), Some(30.0)),
        network_rate("docker0", Some(100.0), Some(100.0)),
    ];
    let counters = Vec::new();

    let all = filtered_network_rates(&rates, &counters, ServerNetworkFilter::All);
    assert_eq!(
        all.iter()
            .map(|rate| rate.name.as_str())
            .collect::<Vec<_>>(),
        vec!["docker0", "enp0s2", "enp0s3", "enp0s1", "lo"]
    );

    let primary = filtered_network_rates(&rates, &counters, ServerNetworkFilter::Primary);
    assert_eq!(
        primary
            .iter()
            .map(|rate| rate.name.as_str())
            .collect::<Vec<_>>(),
        vec!["enp0s2", "enp0s3", "enp0s1"]
    );

    let virtual_rates = filtered_network_rates(&rates, &counters, ServerNetworkFilter::Virtual);
    assert_eq!(
        virtual_rates
            .iter()
            .map(|rate| rate.name.as_str())
            .collect::<Vec<_>>(),
        vec!["docker0"]
    );
}

/// 首次速率样本缺失时按当前收发计数排名，再从非回环接口选择 Primary 回退目标。
///
/// @author kongweiguang
#[test]
fn network_interfaces_rank_counters_before_primary_fallback() {
    let rates = vec![
        network_rate("lo", None, None),
        network_rate("docker0", None, None),
        network_rate("tun0", None, None),
        network_rate("cni0", None, None),
    ];
    let counters = vec![
        NetworkCounterSnapshot {
            name: "lo".to_owned(),
            rx_bytes: 1_000,
            tx_bytes: 0,
        },
        NetworkCounterSnapshot {
            name: "docker0".to_owned(),
            rx_bytes: 100,
            tx_bytes: 0,
        },
        NetworkCounterSnapshot {
            name: "tun0".to_owned(),
            rx_bytes: 101,
            tx_bytes: 0,
        },
        NetworkCounterSnapshot {
            name: "cni0".to_owned(),
            rx_bytes: 10,
            tx_bytes: 0,
        },
    ];

    let all = filtered_network_rates(&rates, &counters, ServerNetworkFilter::All);
    assert_eq!(
        all.iter()
            .map(|rate| rate.name.as_str())
            .collect::<Vec<_>>(),
        vec!["lo", "tun0", "docker0", "cni0"]
    );
    let primary = filtered_network_rates(&rates, &counters, ServerNetworkFilter::Primary);
    assert_eq!(
        primary
            .iter()
            .map(|rate| rate.name.as_str())
            .collect::<Vec<_>>(),
        vec!["tun0"]
    );
}

/// 概览历史只汇总 Primary 接口；缺测方向保留断点，无物理接口时按 main 选择流量最高的非回环接口。
///
/// @author kongweiguang
#[test]
fn server_history_keeps_primary_network_rate_gaps_without_cross_role_fallback() {
    let mut history = ServerInfoHistory::default();
    let mut primary = server_snapshot(ServerInfoTarget::Local, 100);
    primary.network_rates = vec![
        network_rate("lo", Some(1_000.0), Some(1_000.0)),
        network_rate("enp0s1", Some(100.0), None),
        network_rate("enp0s2", None, Some(200.0)),
        network_rate("docker0", Some(500.0), Some(700.0)),
    ];
    assert!(history.record(&primary));
    let sample = history.samples().back().unwrap();
    assert_eq!(sample.receive_bytes_per_second, Some(100.0));
    assert_eq!(sample.transmit_bytes_per_second, Some(200.0));

    let mut partial = server_snapshot(ServerInfoTarget::Local, 200);
    partial.network_rates = vec![
        network_rate("lo", Some(1.0), Some(2.0)),
        network_rate("enp0s1", None, Some(7.0)),
        network_rate("docker0", Some(50.0), Some(60.0)),
    ];
    assert!(history.record(&partial));
    let sample = history.samples().back().unwrap();
    assert_eq!(sample.receive_bytes_per_second, None);
    assert_eq!(sample.transmit_bytes_per_second, Some(7.0));

    let mut virtual_only = server_snapshot(ServerInfoTarget::Local, 300);
    virtual_only.network_rates = vec![
        network_rate("lo", Some(1.0), Some(2.0)),
        network_rate("docker0", Some(20.0), Some(30.0)),
        network_rate("tun0", Some(40.0), Some(50.0)),
    ];
    assert!(history.record(&virtual_only));
    let sample = history.samples().back().unwrap();
    assert_eq!(sample.receive_bytes_per_second, Some(40.0));
    assert_eq!(sample.transmit_bytes_per_second, Some(50.0));
}

/// 目标切换时不把旧主机曲线展示给新容器，缺失 Fast 时间也不会制造样本点。
#[test]
fn server_history_resets_on_target_change_and_requires_fast_sample_time() {
    let mut history = ServerInfoHistory::default();
    assert!(history.record(&server_snapshot(ServerInfoTarget::Local, 10)));

    let container = ServerInfoTarget::Container {
        host_id: None,
        runtime: OperationsContainerRuntime::Podman,
        container_id: "container-a".to_owned(),
    };
    let mut unsampled_container = server_snapshot(container.clone(), 15);
    unsampled_container.sample_times.fast_at_unix_ms = None;
    assert!(!history.record(&unsampled_container));
    assert!(history.samples().is_empty());

    assert!(history.record(&server_snapshot(container.clone(), 20)));
    assert_eq!(history.samples().len(), 1);
    assert_eq!(history.samples().front().unwrap().sampled_at_unix_ms, 20);

    let mut unsampled = server_snapshot(container.clone(), 30);
    unsampled.sample_times.fast_at_unix_ms = None;
    assert!(!history.record(&unsampled));
    assert_eq!(history.samples().len(), 1);
}

/// 搜索同时覆盖容器身份和 Compose 名称，状态筛选只看领域枚举。
#[test]
fn container_search_and_status_filters_use_real_summary_fields() {
    let mut api = container_summary("api", "a1", ContainerStatus::Running, Some("shop"));
    api.target.runtime = ContainerRuntime::Docker;
    api.image = "node:24".to_owned();
    api.ports.push("8080:80/tcp".to_owned());
    let compose = api.compose.as_mut().unwrap();
    compose.service = Some("web".to_owned());
    compose.working_dir = Some("/srv/shop".to_owned());
    compose.config_paths = vec!["/srv/shop/compose.yaml".to_owned()];

    let rows = vec![
        api,
        container_summary("worker", "b2", ContainerStatus::Exited, Some("shop")),
        container_summary("cache", "c3", ContainerStatus::Running, None),
    ];

    let running = filter_containers(&rows, "", ContainerStatusFilter::Running);
    assert_eq!(
        running
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>(),
        ["api", "cache"]
    );
    let project = filter_containers(&rows, "SHOP", ContainerStatusFilter::All);
    assert_eq!(
        project
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>(),
        ["api", "worker"]
    );
    let identity = filter_containers(&rows, "c3", ContainerStatusFilter::All);
    assert_eq!(
        identity
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>(),
        ["cache"]
    );

    for query in [
        "node:24",
        "web",
        "8080:80/tcp",
        "/srv/shop",
        "compose.yaml",
        "docker",
    ] {
        let matches = filter_containers(&rows, query, ContainerStatusFilter::All);
        assert_eq!(
            matches
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            ["api"],
            "query {query:?} should match the container's searchable metadata"
        );
    }
}

/// Compose 容器按项目分组，独立容器仍保留在单独组且顺序稳定。
#[test]
fn container_projects_group_without_merging_standalone_entries() {
    let groups = group_container_projects(vec![
        container_summary("api", "a1", ContainerStatus::Running, Some("shop")),
        container_summary("cache", "c3", ContainerStatus::Running, None),
        container_summary("worker", "b2", ContainerStatus::Exited, Some("shop")),
        container_summary("db", "d4", ContainerStatus::Running, Some("warehouse")),
    ]);

    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0].project.as_deref(), Some("shop"));
    assert_eq!(groups[0].containers.len(), 2);
    assert_eq!(groups[1].project, None);
    assert_eq!(groups[1].containers[0].name, "cache");
    assert_eq!(groups[2].project.as_deref(), Some("warehouse"));
}

/// Compose 分组键逐字段匹配执行目标；相同项目的不同 host、runtime、目录或配置顺序不合并。
///
/// @author kongweiguang
#[test]
fn compose_groups_preserve_the_exact_ordered_execution_target() {
    let mut first = container_summary("api", "a1", ContainerStatus::Running, Some("shop"));
    first.compose.as_mut().unwrap().working_dir = Some("/srv/shop".to_owned());
    first.compose.as_mut().unwrap().config_files =
        vec!["compose.yml".to_owned(), "override.yml".to_owned()];
    let mut same_target = first.clone();
    same_target.name = "worker".to_owned();
    same_target.short_id = "b2".to_owned();
    same_target.target.container_id = "b2-full".to_owned();
    same_target.compose.as_mut().unwrap().service = Some("worker".to_owned());

    let mut other_directory = first.clone();
    other_directory.name = "db".to_owned();
    other_directory.short_id = "c3".to_owned();
    other_directory.target.container_id = "c3-full".to_owned();
    other_directory.compose.as_mut().unwrap().working_dir = Some("/srv/other-shop".to_owned());

    let mut reversed_config_order = first.clone();
    reversed_config_order.name = "cache".to_owned();
    reversed_config_order.short_id = "d4".to_owned();
    reversed_config_order.target.container_id = "d4-full".to_owned();
    reversed_config_order
        .compose
        .as_mut()
        .unwrap()
        .config_files
        .reverse();

    let mut other_host = first.clone();
    other_host.name = "remote".to_owned();
    other_host.short_id = "e5".to_owned();
    other_host.target.container_id = "e5-full".to_owned();
    other_host.target.host_id = Some(HostId::new());

    let mut other_runtime = first.clone();
    other_runtime.name = "podman".to_owned();
    other_runtime.short_id = "f6".to_owned();
    other_runtime.target.container_id = "f6-full".to_owned();
    other_runtime.target.runtime = OperationsContainerRuntime::Docker;

    let groups = group_container_projects(vec![
        first,
        same_target,
        other_directory,
        reversed_config_order,
        other_host,
        other_runtime,
    ]);

    assert_eq!(groups.len(), 5);
    assert_eq!(groups[0].containers.len(), 2);
    assert_eq!(
        groups[0].target,
        Some(ComposeProjectTarget {
            host_id: None,
            runtime: OperationsContainerRuntime::Podman,
            project: "shop".to_owned(),
            working_dir: "/srv/shop".to_owned(),
            config_files: vec!["compose.yml".to_owned(), "override.yml".to_owned()],
        })
    );
    assert_eq!(groups[1].containers[0].name, "db");
    assert_eq!(groups[2].containers[0].name, "cache");
    assert!(groups[3].target.as_ref().unwrap().host_id.is_some());
    assert_eq!(
        groups[4].target.as_ref().unwrap().runtime,
        OperationsContainerRuntime::Docker
    );
}

/// Compose 元数据缺少配置文件身份时分开显示，不把无法证明相同的目标拼成一次操作。
///
/// @author kongweiguang
#[test]
fn compose_rows_without_complete_targets_are_not_merged() {
    let mut first = container_summary("api", "a1", ContainerStatus::Running, Some("shop"));
    first.compose.as_mut().unwrap().config_files.clear();
    let mut second = first.clone();
    second.name = "worker".to_owned();
    second.short_id = "b2".to_owned();
    second.target.container_id = "b2-full".to_owned();

    let groups = group_container_projects(vec![first, second]);
    assert_eq!(groups.len(), 2);
    assert!(groups.iter().all(|group| group.target.is_none()));
    assert!(groups.iter().all(|group| group.containers.len() == 1));
}

/// 时间按真实毫秒间距映射，百分比固定在 0–100 而不按窗口极值缩放。
///
/// @author kongweiguang
#[test]
fn metric_plot_uses_real_time_and_absolute_percentage_scale() {
    assert_eq!(
        time_series_position(150, 25.0, 100, 200, 0.0, 100.0),
        Some((0.5, 0.75))
    );
    assert_eq!(
        time_series_position(150, 50.0, 100, 200, 0.0, 100.0),
        Some((0.5, 0.5))
    );
    assert_eq!(time_series_position(150, 50.0, 100, 200, 1.0, 1.0), None);
    assert_eq!(time_series_position(150, -1.0, 100, 200, 0.0, 100.0), None);
    assert_eq!(time_series_position(150, 101.0, 100, 200, 0.0, 100.0), None);
}

/// 网络速率沿用 main 的 KB/MB 标签，未建立差分基线时显示采样中。
///
/// @author kongweiguang
#[test]
fn network_rate_labels_keep_units_and_unknown_state() {
    assert_eq!(network_rate_label(Some(512.0)), "512 B/s");
    assert_eq!(network_rate_label(Some(1536.0)), "1.5 KB/s");
    assert_eq!(network_rate_label(Some(1_572_864.0)), "1.5 MB/s");
    assert_eq!(network_rate_label(Some(1_610_612_736.0)), "1.5 GB/s");
    assert_eq!(network_rate_label(Some(1_649_267_441_664.0)), "1.5 TB/s");
    assert_eq!(
        network_rate_label(None),
        crate::locale::text("采样中", "Sampling…").to_string()
    );
    assert_eq!(network_rate_label(Some(f64::NAN)), "—");
}

/// 慢速曲线只追加新层级回执，并在探针不可用时保留真实断点。
/// 测试直接读取子模块可见的私有状态，避免只供测试的访问器进入生产 API。
#[test]
fn server_history_keeps_slow_tiers_independent_and_gaps_missing_reads() {
    let mut history = ServerInfoHistory::default();
    let mut first = server_snapshot(ServerInfoTarget::Local, 100);
    first.sample_times.slow_at_unix_ms = Some(80);
    first.sample_times.gpu_at_unix_ms = Some(70);
    first.disk_probe_status = ServerInfoProbeStatus::Available;
    first.disks.push(DiskSnapshot {
        filesystem: "/dev/test".to_owned(),
        mount: "/".to_owned(),
        total_bytes: Some(100),
        used_bytes: Some(40),
        available_bytes: Some(60),
    });
    first.gpu_probe_status = ServerInfoProbeStatus::Available;
    first.gpus = vec![
        gpu_snapshot("gpu-0", Some(30.0)),
        gpu_snapshot("gpu-1", Some(70.0)),
    ];
    assert!(history.record(&first));
    assert_eq!(history.disk_mount.as_deref(), Some("/"));
    assert_eq!(history.disk_samples.back().unwrap().value, Some(40.0));
    assert_eq!(history.gpu_samples.back().unwrap().value, Some(50.0));

    let mut fast_only = server_snapshot(ServerInfoTarget::Local, 200);
    fast_only.sample_times.slow_at_unix_ms = Some(80);
    fast_only.sample_times.gpu_at_unix_ms = Some(70);
    assert!(history.record(&fast_only));
    assert_eq!(history.disk_samples.len(), 1);
    assert_eq!(history.gpu_samples.len(), 1);

    let mut missing = server_snapshot(ServerInfoTarget::Local, 300);
    missing.sample_times.slow_at_unix_ms = Some(280);
    missing.sample_times.gpu_at_unix_ms = Some(270);
    missing.disk_probe_status =
        ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::PermissionDenied);
    missing.gpu_probe_status = ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::NotFound);
    assert!(history.record(&missing));
    assert_eq!(history.disk_samples.back().unwrap().value, None);
    assert_eq!(history.gpu_samples.back().unwrap().value, None);
}

/// 越界百分比不被夹为 0 或 100，趋势中的该点应保留为缺口。
/// 测试直接读取子模块可见的私有状态，避免只供测试的访问器进入生产 API。
///
/// @author kongweiguang
#[test]
fn server_history_rejects_out_of_range_percentage_samples() {
    let mut snapshot = server_snapshot(ServerInfoTarget::Local, 100);
    snapshot.cpu.usage_percent = Some(101.0);
    snapshot.sample_times.slow_at_unix_ms = Some(100);
    snapshot.disk_probe_status = ServerInfoProbeStatus::Available;
    snapshot.disks.push(DiskSnapshot {
        filesystem: "/dev/test".to_owned(),
        mount: "/".to_owned(),
        total_bytes: Some(100),
        used_bytes: Some(101),
        available_bytes: Some(0),
    });
    snapshot.sample_times.gpu_at_unix_ms = Some(100);
    snapshot.gpu_probe_status = ServerInfoProbeStatus::Available;
    snapshot.gpus = vec![gpu_snapshot("gpu-0", Some(-1.0))];

    let mut history = ServerInfoHistory::default();
    assert!(history.record(&snapshot));
    assert_eq!(history.samples().back().unwrap().cpu_percent, None);
    assert_eq!(history.disk_samples.back().unwrap().value, None);
    assert_eq!(history.gpu_samples.back().unwrap().value, None);
}

/// 为监控历史提供包含真实层级时间的最小数据样本。
///
/// @author kongweiguang
fn server_snapshot(target: ServerInfoTarget, sampled_at_unix_ms: u64) -> ServerInfoSnapshot {
    ServerInfoSnapshot {
        target,
        captured_at_unix_ms: sampled_at_unix_ms,
        sample_times: ServerInfoSampleTimes {
            fast_at_unix_ms: Some(sampled_at_unix_ms),
            ..ServerInfoSampleTimes::default()
        },
        uptime_seconds: None,
        hostname: None,
        operating_system: None,
        architecture: None,
        kernel: None,
        cpu: CpuSnapshot {
            usage_percent: Some(sampled_at_unix_ms as f64),
            ..CpuSnapshot::default()
        },
        memory: MemorySnapshot {
            used_bytes: Some(50),
            total_bytes: Some(100),
            ..MemorySnapshot::default()
        },
        disks: Vec::new(),
        network_counters: Vec::new(),
        network_rates: Vec::new(),
        process_count: None,
        running_process_count: None,
        processes: Vec::new(),
        disk_probe_status: ServerInfoProbeStatus::NotSampled,
        network_probe_status: ServerInfoProbeStatus::NotSampled,
        process_probe_status: ServerInfoProbeStatus::NotSampled,
        gpu_probe_status: ServerInfoProbeStatus::NotSampled,
        gpus: Vec::new(),
        npu_probe_status: ServerInfoProbeStatus::NotSampled,
        npus: Vec::new(),
    }
}

/// 构造具有真实利用率读数的设备样本，供历史聚合测试使用。
fn gpu_snapshot(id: &str, utilization_percent: Option<f64>) -> GpuSnapshot {
    GpuSnapshot {
        id: id.to_owned(),
        model: None,
        vendor: None,
        driver_version: None,
        utilization_percent,
        memory_total_bytes: None,
        memory_used_bytes: None,
        temperature_celsius: None,
        power_watts: None,
    }
}

/// 为网络历史回归构造单个接口读数，并允许独立模拟收发缺测。
///
/// @author kongweiguang
fn network_rate(
    name: &str,
    rx_bytes_per_second: Option<f64>,
    tx_bytes_per_second: Option<f64>,
) -> NetworkRateSnapshot {
    NetworkRateSnapshot {
        name: name.to_owned(),
        rx_bytes_per_second,
        tx_bytes_per_second,
        counter_reset: false,
    }
}

/// 为筛选和分组测试构造含完整 Compose 目标字段的摘要，使分组身份可被真实断言。
///
/// @author kongweiguang
fn container_summary(
    name: &str,
    short_id: &str,
    status: ContainerStatus,
    project: Option<&str>,
) -> ContainerSummary {
    ContainerSummary {
        target: ContainerTarget {
            host_id: None,
            runtime: ContainerRuntime::Podman,
            container_id: format!("{short_id}-full"),
        },
        short_id: short_id.to_owned(),
        name: name.to_owned(),
        image: "alpine:latest".to_owned(),
        status,
        status_text: match status {
            ContainerStatus::Running => "Up".to_owned(),
            ContainerStatus::Exited => "Exited".to_owned(),
            _ => "Other".to_owned(),
        },
        ports: Vec::new(),
        labels: Vec::new(),
        compose: project.map(|project| ComposeMetadata {
            project: Some(project.to_owned()),
            service: Some(name.to_owned()),
            working_dir: Some(format!("/work/{project}")),
            config_files: vec![format!("{project}.yaml")],
            ..ComposeMetadata::default()
        }),
    }
}

/// 同目标详情读取取消后保留成功快照，并能在用户显式重试时回到 loading。
///
/// @author kongweiguang
#[test]
fn detail_read_cancel_preserves_snapshot_and_same_target_retry() {
    let mut detail = DetailReadState::<&str, &str>::default();
    detail.begin("container-a");
    assert!(detail.complete(&"container-a", Ok("last success")));
    assert_eq!(detail.phase(), DetailReadPhase::Ready);
    assert_eq!(detail.data(), Some(&"last success"));

    detail.begin("container-a");
    assert!(detail.cancel("hidden".to_owned()));
    assert_eq!(detail.phase(), DetailReadPhase::Cancelled);
    assert_eq!(detail.feedback(), Some("hidden"));
    assert_eq!(detail.data(), Some(&"last success"));
    assert!(!detail.complete(&"container-a", Ok("late result")));
    assert!(!detail.cancel("not loading".to_owned()));

    detail.begin("container-a");
    assert_eq!(detail.phase(), DetailReadPhase::Loading);
    assert_eq!(detail.data(), Some(&"last success"));
    assert!(detail.complete(&"container-a", Err("temporary failure".to_owned())));
    assert_eq!(detail.phase(), DetailReadPhase::Failed);
    assert_eq!(detail.data(), Some(&"last success"));
}
/// 日志和 stats 是不同读取目标，同一容器切换查询类型后不展示错误快照。
///
/// @author kongweiguang
#[test]
fn container_output_kind_change_clears_previous_snapshot() {
    let target = ContainerTarget {
        host_id: None,
        runtime: ContainerRuntime::Docker,
        container_id: "container-a".to_owned(),
    };
    let logs = ContainerOutputTarget {
        target: target.clone(),
        kind: ContainerOutputKind::Logs,
    };
    let stats = ContainerOutputTarget {
        target,
        kind: ContainerOutputKind::Stats,
    };
    let mut detail = DetailReadState::<ContainerOutputTarget, String>::default();

    detail.begin(logs.clone());
    assert!(detail.complete(&logs, Ok("log snapshot".to_owned())));
    detail.begin(stats.clone());

    assert_eq!(detail.data(), None);
    assert_eq!(detail.phase(), DetailReadPhase::Loading);
    assert!(detail.complete(&stats, Ok("stats snapshot".to_owned())));
    assert_eq!(detail.data().map(String::as_str), Some("stats snapshot"));
}
/// 详情目标切换后立即丢弃旧投影，旧回执不能填充新目标的显示区域。
///
/// @author kongweiguang
#[test]
fn detail_read_target_change_clears_snapshot_and_rejects_stale_receipt() {
    let mut detail = DetailReadState::<&str, &str>::default();
    detail.begin("container-a");
    assert!(detail.complete(&"container-a", Ok("old target")));
    detail.begin("container-b");

    assert_eq!(detail.data(), None);
    assert!(!detail.complete(&"container-a", Ok("stale")));
    assert_eq!(detail.phase(), DetailReadPhase::Loading);
    assert!(detail.complete(&"container-b", Err("read failed".to_owned())));
    assert_eq!(detail.phase(), DetailReadPhase::Failed);
    assert_eq!(detail.feedback(), Some("read failed"));
}

/// 隐藏面板时撤销实际 tmux 详情任务、结束 loading 并保留已有结果供重入查看。
///
/// @author kongweiguang
#[test]
fn tmux_detail_hide_cancels_only_pending_reads_and_preserves_snapshots() {
    use tokio_util::sync::CancellationToken;

    let list_cancellation = CancellationToken::new();
    let mut detail = TmuxDetailState {
        session_id: Some("$1".to_owned()),
        windows: Some(Vec::new()),
        windows_loading: true,
        panes: Some(Vec::new()),
        capture: Some(kerminal_domain::operations::TmuxPaneCapture {
            pane_id: "%1".to_owned(),
            text: "last capture".to_owned(),
            captured_lines: 1,
            truncated: false,
        }),
        capture_loading: true,
        cancellation: Some(list_cancellation.clone()),
        ..TmuxDetailState::default()
    };
    let capture_ticket = detail.capture_request.begin();
    let capture_cancellation = capture_ticket.cancellation.clone();

    detail.cancel_pending_reads();

    assert!(list_cancellation.is_cancelled());
    assert!(capture_cancellation.is_cancelled());
    assert!(!detail.windows_loading);
    assert!(!detail.panes_loading);
    assert!(!detail.capture_loading);
    assert!(detail.windows_error.is_some());
    assert!(detail.capture_error.is_some());
    assert_eq!(detail.windows, Some(Vec::new()));
    assert_eq!(detail.panes, Some(Vec::new()));
    assert_eq!(
        detail.capture.as_ref().map(|capture| capture.text.as_str()),
        Some("last capture")
    );
}

/// 对已结束的 tmux 详情读取再次隐藏不改写完成快照或制造取消提示。
///
/// @author kongweiguang
#[test]
fn tmux_detail_hide_leaves_completed_snapshot_unchanged() {
    let mut detail = TmuxDetailState {
        session_id: Some("$1".to_owned()),
        windows: Some(Vec::new()),
        panes: Some(Vec::new()),
        capture: Some(kerminal_domain::operations::TmuxPaneCapture {
            pane_id: "%1".to_owned(),
            text: "complete".to_owned(),
            captured_lines: 1,
            truncated: false,
        }),
        ..TmuxDetailState::default()
    };
    let completed = detail.capture_request.begin();
    let completed_cancellation = completed.cancellation.clone();
    assert!(detail.capture_request.complete(&completed));

    detail.cancel_pending_reads();

    assert!(!detail.windows_loading);
    assert!(!detail.panes_loading);
    assert!(!detail.capture_loading);
    assert_eq!(detail.windows_error, None);
    assert_eq!(detail.panes_error, None);
    assert_eq!(detail.capture_error, None);
    assert!(!completed_cancellation.is_cancelled());
    assert_eq!(
        detail.capture.as_ref().map(|capture| capture.text.as_str()),
        Some("complete")
    );
}

/// 写回执暂存按完整目标和操作身份去重，同目标重试覆盖旧提示，容量固定且不累积历史。
/// 消费只取走目标匹配的提示一次，其它目标的失败或未知结果不会被串走。
///
/// @author kongweiguang
#[test]
fn deferred_tmux_feedback_is_bounded_identity_scoped_and_consumed_once() {
    let target_a = TmuxTarget {
        host_id: Some(HostId::new()),
        ..TmuxTarget::default()
    };
    let target_b = TmuxTarget {
        host_id: Some(HostId::new()),
        ..TmuxTarget::default()
    };
    let mut queue = VecDeque::new();
    let mut losses = 0usize;

    defer_tmux_feedback(
        &mut queue,
        &mut losses,
        TmuxMutationFeedback {
            target: target_a.clone(),
            identity: TmuxMutationIdentity::Kill {
                session_id: "$1".to_owned(),
            },
            message: "first".to_owned(),
        },
    );
    // 同目标同操作的迟到重试覆盖旧文案，不累积两条冲突提示；被覆盖的旧结果
    // 尚未展示，必须计入显式丢失，不能静默消失。
    defer_tmux_feedback(
        &mut queue,
        &mut losses,
        TmuxMutationFeedback {
            target: target_a.clone(),
            identity: TmuxMutationIdentity::Kill {
                session_id: "$1".to_owned(),
            },
            message: "retry".to_owned(),
        },
    );
    assert_eq!(losses, 1);
    defer_tmux_feedback(
        &mut queue,
        &mut losses,
        TmuxMutationFeedback {
            target: target_b.clone(),
            identity: TmuxMutationIdentity::Detach {
                session_id: "$2".to_owned(),
            },
            message: "other".to_owned(),
        },
    );
    assert_eq!(losses, 1);
    assert_eq!(queue.len(), 2);

    // 消费只取走 A 的提示，B 的失败留给 B 返回时显示。
    assert_eq!(
        take_deferred_tmux_feedback(&mut queue, &target_a).as_deref(),
        Some("retry")
    );
    assert_eq!(queue.len(), 1);
    assert!(take_deferred_tmux_feedback(&mut queue, &target_a).is_none());

    // 不同操作身份各自保留；容量上限按 FIFO 淘汰最早条目，被淘汰条目计入显式丢失。
    // 循环开始前队列里仍有 B 的一条 Detach，它会先于本轮新增条目被淘汰；只按“本轮
    // 溢出数”断言会漏掉这条从未展示的既有结果，因此丢失增量必须包含它。
    let before = losses;
    let queued_before = queue.len();
    assert_eq!(queued_before, 1, "target B entry must still be queued");
    let inserted = TMUX_DEFERRED_FEEDBACK_CAPACITY + 3;
    for index in 0..inserted {
        defer_tmux_feedback(
            &mut queue,
            &mut losses,
            TmuxMutationFeedback {
                target: target_a.clone(),
                identity: TmuxMutationIdentity::Rename {
                    session_id: format!("${index}"),
                },
                message: format!("rename {index}"),
            },
        );
    }
    assert_eq!(queue.len(), TMUX_DEFERRED_FEEDBACK_CAPACITY);
    // 既有 B 条目 + 本轮插入条目超出容量的部分都要计入丢失。
    let evicted = queued_before + inserted - TMUX_DEFERRED_FEEDBACK_CAPACITY;
    assert_eq!(
        losses - before,
        evicted,
        "pre-existing and overflowing entries are both counted as lost"
    );
    assert!(
        queue.iter().all(|entry| entry.target == target_a),
        "oldest entry (target B) is evicted first once capacity is exceeded"
    );
    let consumed = take_deferred_tmux_feedback(&mut queue, &target_a).expect("queued renames");
    // 被淘汰的既有条目不属于本轮 rename，因此本轮 rename 的起点是“被淘汰数 - 既有条数”。
    let first_kept = evicted - queued_before;
    let expected = (first_kept..inserted)
        .map(|index| format!("rename {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        consumed, expected,
        "FIFO keeps exactly the newest entries in arrival order"
    );
    assert!(queue.is_empty());

    // 丢失只累计条数，不保存被丢文案；提示只报数量与原因，不伪造结果归属，
    // 断言同时接受双语文案，不预设应用 locale。
    let notice = super::tmux_feedback_loss_notice(3);
    assert!(notice.contains('3'));
    assert!(notice.contains("未展示") || notice.contains("were not shown"));
    assert!(!notice.contains("rename 0"));
    assert!(!notice.contains("retry"));
}
