//! CommandsView 只绑定当前 Ready pane 的真实 UI owner 和 generation。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
};

use kerminal_application::{
    sessions::UiSessionBinding,
    workspace::{
        WorkspaceLocalMachineScope, WorkspaceMachineStatus, WorkspacePaneMode,
        WorkspaceSessionPane, WorkspaceSessionSnapshot, WorkspaceSessionTab, WorkspaceTargetRef,
    },
};
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::{LocalSessionTarget, SessionOwner, SessionTarget, TerminalSize},
    workspace::{SplitDirection, TerminalLayoutNode},
};

use crate::root::{RootView, logical_input_modal_blocked, workspace_runtime::WorkspaceRuntime};

/// 构造最小本地 pane，使测试经过与实际 workspace 相同的布局校验。
///
/// @author kongweiguang
fn pane(id: PaneId) -> WorkspaceSessionPane {
    WorkspaceSessionPane {
        id,
        title: "shell".to_owned(),
        machine_id: "local".to_owned(),
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
        status: WorkspaceMachineStatus::Online,
        latency_ms: None,
        local_machine_scope: Some(WorkspaceLocalMachineScope::Workspace),
        output_history: None,
    }
}

/// 创建 terminal tab 快照；活动 tab 与 pane 焦点分别可控以验证 Root 不跨 tab 猜目标。
///
/// @author kongweiguang
fn snapshot(
    active_tab_id: TabId,
    focused_pane_id: PaneId,
    terminal_tabs: Vec<WorkspaceSessionTab>,
    terminal_panes: Vec<WorkspaceSessionPane>,
) -> WorkspaceSessionSnapshot {
    WorkspaceSessionSnapshot {
        pinned_containers: Vec::new(),
        active_tab_id: Some(active_tab_id),
        focused_pane_id: Some(focused_pane_id),
        selected_machine_id: None,
        terminal_tabs,
        terminal_panes,
        tab_groups: BTreeMap::new(),
    }
}

/// 构造只在 runtime 安装 session 时使用的本地目标，不涉及真实进程启动。
///
/// @author kongweiguang
fn local_target() -> SessionTarget {
    SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("fixture-shell"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    })
}

/// 构造与 pane、session、代次三者完全一致的输入 binding。
///
/// @author kongweiguang
fn binding(
    session_id: SessionId,
    tab_id: TabId,
    pane_id: PaneId,
    session_generation: u64,
) -> UiSessionBinding {
    UiSessionBinding {
        session_id,
        owner: SessionOwner::Ui { tab_id, pane_id },
        session_generation,
    }
}

/// 校验活动 tab 和 focused pane 精确决定目标，其他 tab 的有效 session 不会被选中。
///
/// @author kongweiguang
#[test]
fn resolves_only_the_focused_ready_pane_in_the_active_tab() {
    let active_tab = TabId::new();
    let other_tab = TabId::new();
    let first_pane = PaneId::new();
    let focused_pane = PaneId::new();
    let other_tab_pane = PaneId::new();
    let first_session = SessionId::new();
    let focused_session = SessionId::new();
    let other_session = SessionId::new();

    let active_layout = TerminalLayoutNode::pane(first_pane.clone())
        .split_pane(
            &first_pane,
            focused_pane.clone(),
            SplitDirection::Vertical,
            "active-split",
            false,
        )
        .expect("valid split layout");
    let workspace = snapshot(
        active_tab.clone(),
        focused_pane.clone(),
        vec![
            WorkspaceSessionTab::Terminal {
                id: active_tab.clone(),
                tab_group_id: None,
                title: "active".to_owned(),
                machine_id: "local".to_owned(),
                layout: active_layout,
            },
            WorkspaceSessionTab::Terminal {
                id: other_tab.clone(),
                tab_group_id: None,
                title: "other".to_owned(),
                machine_id: "local".to_owned(),
                layout: TerminalLayoutNode::pane(other_tab_pane.clone()),
            },
        ],
        vec![
            pane(first_pane.clone()),
            pane(focused_pane.clone()),
            pane(other_tab_pane.clone()),
        ],
    );
    let mut runtime = WorkspaceRuntime::from_snapshot(&workspace).expect("valid workspace");
    for (pane_id, session_id) in [
        (first_pane.clone(), first_session.clone()),
        (focused_pane.clone(), focused_session.clone()),
        (other_tab_pane.clone(), other_session.clone()),
    ] {
        runtime
            .install_session(
                &pane_id,
                session_id,
                local_target(),
                TerminalSize::default(),
            )
            .expect("fixture session is installed");
    }

    let registrations = HashMap::from([
        (
            first_session.clone(),
            binding(first_session.clone(), active_tab.clone(), first_pane, 1),
        ),
        (
            focused_session.clone(),
            binding(
                focused_session.clone(),
                active_tab.clone(),
                focused_pane.clone(),
                7,
            ),
        ),
        (
            other_session.clone(),
            binding(other_session.clone(), other_tab, other_tab_pane, 3),
        ),
    ]);

    let selected = RootView::resolve_active_commands_binding(&workspace, &runtime, |session_id| {
        registrations.get(session_id)
    })
    .expect("the focused Ready pane has a validated binding");
    assert_eq!(selected.session_id, focused_session);
    assert_eq!(selected.session_generation, 7);
    assert_eq!(
        selected.owner,
        SessionOwner::Ui {
            tab_id: active_tab.clone(),
            pane_id: focused_pane.clone(),
        }
    );

    let no_active_handler =
        RootView::resolve_active_commands_binding(&workspace, &runtime, |session_id| {
            (session_id == &other_session)
                .then(|| registrations.get(session_id))
                .flatten()
        });
    assert!(
        no_active_handler.is_none(),
        "a valid handler in another tab must not become an implicit fallback"
    );
}

/// 重连后的同一 SessionId 使用新 handler generation；pane 关闭态立即失去可执行 binding。
///
/// @author kongweiguang
#[test]
fn refreshes_generation_and_clears_binding_when_pane_disconnects() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let session_id = SessionId::new();
    let workspace = snapshot(
        tab_id.clone(),
        pane_id.clone(),
        vec![WorkspaceSessionTab::Terminal {
            id: tab_id.clone(),
            tab_group_id: None,
            title: "active".to_owned(),
            machine_id: "local".to_owned(),
            layout: TerminalLayoutNode::pane(pane_id.clone()),
        }],
        vec![pane(pane_id.clone())],
    );
    let mut runtime = WorkspaceRuntime::from_snapshot(&workspace).expect("valid workspace");
    runtime
        .install_session(
            &pane_id,
            session_id.clone(),
            local_target(),
            TerminalSize::default(),
        )
        .expect("fixture session is installed");
    let mut registrations = HashMap::from([(
        session_id.clone(),
        binding(session_id.clone(), tab_id.clone(), pane_id.clone(), 10),
    )]);

    let first = RootView::resolve_active_commands_binding(&workspace, &runtime, |session_id| {
        registrations.get(session_id)
    })
    .expect("active handler is available");
    assert_eq!(first.session_generation, 10);

    registrations.insert(
        session_id.clone(),
        binding(session_id.clone(), tab_id, pane_id.clone(), 11),
    );
    let reconnected =
        RootView::resolve_active_commands_binding(&workspace, &runtime, |session_id| {
            registrations.get(session_id)
        })
        .expect("the replacement handler is read from the registration map");
    assert_eq!(reconnected.session_id, session_id);
    assert_eq!(reconnected.session_generation, 11);

    runtime
        .begin_close(&pane_id)
        .expect("disconnect/close invalidates the Ready phase");
    assert!(
        RootView::resolve_active_commands_binding(&workspace, &runtime, |session_id| {
            registrations.get(session_id)
        })
        .is_none()
    );
}

/// 拒绝 session ID 相同但 owner 指向别的 pane 的陈旧或错配 handler。
///
/// @author kongweiguang
#[test]
fn rejects_a_binding_whose_owner_does_not_match_the_active_pane() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let wrong_pane_id = PaneId::new();
    let session_id = SessionId::new();
    let workspace = snapshot(
        tab_id.clone(),
        pane_id.clone(),
        vec![WorkspaceSessionTab::Terminal {
            id: tab_id.clone(),
            tab_group_id: None,
            title: "active".to_owned(),
            machine_id: "local".to_owned(),
            layout: TerminalLayoutNode::pane(pane_id.clone()),
        }],
        vec![pane(pane_id.clone())],
    );
    let mut runtime = WorkspaceRuntime::from_snapshot(&workspace).expect("valid workspace");
    runtime
        .install_session(
            &pane_id,
            session_id.clone(),
            local_target(),
            TerminalSize::default(),
        )
        .expect("fixture session is installed");
    let registrations = HashMap::from([(
        session_id.clone(),
        binding(session_id, tab_id, wrong_pane_id, 4),
    )]);

    assert!(
        RootView::resolve_active_commands_binding(&workspace, &runtime, |session_id| {
            registrations.get(session_id)
        })
        .is_none()
    );
}

/// 命令面板名称来自绑定所属 tab 与 pane；改名只改变投影，不换 session generation。
///
/// @author kongweiguang
#[test]
fn target_presentation_tracks_tab_rename_and_merges_identical_names() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let session_id = SessionId::new();
    let active_binding = binding(session_id, tab_id.clone(), pane_id.clone(), 7);
    let original = snapshot(
        tab_id.clone(),
        pane_id.clone(),
        vec![WorkspaceSessionTab::Terminal {
            id: tab_id.clone(),
            tab_group_id: None,
            title: "Workspace".to_owned(),
            machine_id: "local".to_owned(),
            layout: TerminalLayoutNode::pane(pane_id.clone()),
        }],
        vec![pane(pane_id.clone())],
    );

    let original_presentation =
        RootView::resolve_commands_target_presentation(&active_binding, &original, |_| {
            "PowerShell".to_owned()
        })
        .expect("the same active terminal tab owns the pane");
    assert_eq!(original_presentation.label, "Workspace · PowerShell");

    let renamed = snapshot(
        tab_id.clone(),
        pane_id.clone(),
        vec![WorkspaceSessionTab::Terminal {
            id: tab_id.clone(),
            tab_group_id: None,
            title: "Project".to_owned(),
            machine_id: "local".to_owned(),
            layout: TerminalLayoutNode::pane(pane_id.clone()),
        }],
        vec![pane(pane_id.clone())],
    );
    let renamed_presentation =
        RootView::resolve_commands_target_presentation(&active_binding, &renamed, |_| {
            "PowerShell".to_owned()
        })
        .expect("rename changes only the target label projection");
    assert_eq!(renamed_presentation.label, "Project · PowerShell");
    assert_eq!(renamed_presentation.binding, original_presentation.binding);

    let identical_names =
        RootView::resolve_commands_target_presentation(&active_binding, &renamed, |_| {
            "Project".to_owned()
        })
        .expect("the active binding still owns the renamed pane");
    assert_eq!(identical_names.label, "Project");

    let wrong_pane = binding(
        active_binding.session_id.clone(),
        tab_id,
        PaneId::new(),
        active_binding.session_generation,
    );
    assert!(
        RootView::resolve_commands_target_presentation(&wrong_pane, &renamed, |_| {
            "should not be used".to_owned()
        },)
        .is_none()
    );
}

/// Snippet 与根层危险确认均抑制迟到 pane 输入，不改变 Commands 持有的精确 session binding。
///
/// @author kongweiguang
#[test]
fn snippet_editor_modal_blocks_logical_pane_input_routing() {
    assert!(logical_input_modal_blocked(
        false, false, true, false, false
    ));
    assert!(!logical_input_modal_blocked(
        false, false, false, false, false
    ));
    assert!(logical_input_modal_blocked(
        false, false, false, true, false
    ));
    assert!(logical_input_modal_blocked(
        false, false, false, false, true
    ));
}
