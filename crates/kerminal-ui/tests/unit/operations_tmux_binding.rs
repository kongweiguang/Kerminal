//! 固定 tmux 的 setter 级回归：同 Tab 换 pane 保活、目标变化清空旧数据，
//! 以及 SSH host 切换按 Root 的生产顺序先重置宿主级查询再绑定新 pane。
//!
//! 纯决策函数无法证明视图代次、确认清理和详情取消等 setter 副作用；这里用真实
//! `OperationsView` 与本地面板端口跑同一路径，不连接真实 SSH 或 tmux。
//!
//! @author kongweiguang

#[path = "../support/operations_ports.rs"]
mod ports;

use std::sync::Arc;

use gpui_kit::{AppContext as _, Entity, TestAppContext};
use kerminal_application::sessions::UiSessionBinding;
use kerminal_application::{ApplicationTaskExecutor, operations::TmuxCapability};
use kerminal_domain::{
    identity::{HostId, PaneId, SessionId, TabId},
    operations::{TmuxPaneCapture, TmuxSession},
    sessions::SessionOwner,
};
use tokio_util::sync::CancellationToken;

use super::{
    OperationsSection, OperationsView, TmuxAttachTarget, TmuxAttachTerminalBinding,
    TmuxDetachIntent, TmuxDetailState, TmuxMutationIdentity, TmuxSnapshot, ToolRailOperationTarget,
    state::SectionPhase,
};

/// 构造真实 GPUI 实体并保留测试 runtime；执行器只接收显式失败端口。
///
/// @author kongweiguang
fn panel(app: &mut TestAppContext) -> (tokio::runtime::Runtime, Entity<OperationsView>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tmux binding test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let operations = ports::operations_service();
    let view =
        app.new(|_| OperationsView::new_panel(operations, executor, OperationsSection::Tmux));
    (runtime, view)
}

/// 用只回答明确 tmux argv 的内存脚本装配真实 View/service，驱动完整异步回执。
///
/// 其它命令、SSH、终端和容器端口继续显式失败，测试不会启动进程或网络。
///
/// @author kongweiguang
fn panel_with_commands(
    app: &mut TestAppContext,
    script: ports::TmuxCommandScript,
) -> (tokio::runtime::Runtime, Entity<OperationsView>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tmux command test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let operations = ports::operations_service_with_commands(Arc::new(script));
    let view =
        app.new(|_| OperationsView::new_panel(operations, executor, OperationsSection::Tmux));
    (runtime, view)
}

/// 用同一内存命令端口装配完整运维页，验证中央页也遵循当前目标重置语义。
///
/// @author kongweiguang
fn dashboard_with_commands(
    app: &mut TestAppContext,
    script: ports::TmuxCommandScript,
) -> (tokio::runtime::Runtime, Entity<OperationsView>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tmux dashboard test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let operations = ports::operations_service_with_commands(Arc::new(script));
    let view = app.new(|_| OperationsView::new(operations, executor));
    (runtime, view)
}

/// 交替推进 GPUI 和其独立 Tokio runtime，让真实 command 回执写入生产 View。
///
/// 固定轮次用于短小内存回执；需要暂停/释放的测试另行等待明确门闩。
///
/// @author kongweiguang
fn pump(app: &mut TestAppContext, runtime: &tokio::runtime::Runtime) {
    for _ in 0..12 {
        app.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
}

/// 推进到生产 View 达到明确终态，避免用固定轮次猜测 kill/刷新是否已完成。
///
/// 回执由真实内存命令端口产生，条件只读取 View 的当前状态，不直接修改它。
///
/// @author kongweiguang
fn pump_until(
    app: &mut TestAppContext,
    runtime: &tokio::runtime::Runtime,
    view: &Entity<OperationsView>,
    mut done: impl FnMut(&OperationsView) -> bool,
) {
    for _ in 0..200 {
        app.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
        if view.update(app, |view, _| done(view)) {
            return;
        }
    }
    panic!("production tmux async receipt did not reach the expected state");
}

/// 构造带创建时间的会话身份，供 async kill 门禁和确认匹配回归使用。
///
/// @author kongweiguang
fn tmux_session_with_identity(
    id: &str,
    name: &str,
    target_ref: &str,
    created_at: u64,
) -> TmuxSession {
    TmuxSession {
        id: id.to_owned(),
        name: name.to_owned(),
        attached: false,
        clients: 0,
        windows: 1,
        current_path: None,
        created_at: Some(created_at),
        activity_at: Some(created_at),
        target_ref: target_ref.to_owned(),
    }
}

/// 准备已激活的固定 tmux Local 面板，让真实 `load_tmux` 可以直接发起异步读取。
///
/// 本机 profile 目标会经生产 `resolve_local_context` 解析本地主机 ID；未提供时只走
/// 默认模板（空目录即无本机上下文），因此需要真实本机用例时调用方必须同时装配主机服务。
///
/// @author kongweiguang
fn prepared_local_panel(
    app: &mut TestAppContext,
    script: ports::TmuxCommandScript,
    profile_id: Option<&str>,
) -> (
    tokio::runtime::Runtime,
    Entity<OperationsView>,
    TabId,
    TmuxAttachTerminalBinding,
) {
    prepared_local_panel_with_service(app, script, profile_id, None)
}

/// 允许本机 profile 用例注入已登记本地主机的真实服务；其余面板装配保持同一路径。
///
/// @author kongweiguang
fn prepared_local_panel_with_service(
    app: &mut TestAppContext,
    script: ports::TmuxCommandScript,
    profile_id: Option<&str>,
    local_hosts: Option<Vec<ports::ManagedLocalHost>>,
) -> (
    tokio::runtime::Runtime,
    Entity<OperationsView>,
    TabId,
    TmuxAttachTerminalBinding,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tmux command test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let operations = match local_hosts {
        Some(hosts) => ports::operations_service_with_local_hosts(Arc::new(script), hosts),
        None => ports::operations_service_with_commands(Arc::new(script)),
    };
    let view =
        app.new(|_| OperationsView::new_panel(operations, executor, OperationsSection::Tmux));
    let tab_id = TabId::new();
    let attach = binding(&tab_id, None, profile_id, PaneId::new());
    view.update(app, |view, _| {
        view.active = true;
        view.initialized = true;
        view.tool_rail_target = Some(ToolRailOperationTarget::Local);
        view.tmux_attach_binding = Some(attach.clone());
    });
    (runtime, view, tab_id, attach)
}

/// 为 async kill 回归布置 Ready 列表、详情和已确认弹窗，模拟提交后立刻换 pane。
///
/// 所有下一步动作仍调用生产 `kill_tmux_session`/setter，测试只提供初始 UI 投影。
///
/// @author kongweiguang
fn seed_kill_and_switch_pane(
    view: &mut OperationsView,
    target_ref: &str,
    next: &TmuxAttachTerminalBinding,
    cx: &mut gpui_kit::Context<OperationsView>,
) {
    let session = tmux_session_with_identity("$1", "work", target_ref, 100);
    let ready = view.tmux.begin(view.target_generation);
    assert!(view.tmux.complete(
        &ready,
        Ok(TmuxSnapshot {
            capability: TmuxCapability {
                target_ref: target_ref.to_owned(),
                available: true,
                version: Some("tmux 3.4".to_owned()),
                reason: None,
            },
            sessions: vec![session.clone()],
        }),
    ));
    view.tmux_details = TmuxDetailState {
        session_id: Some("$1".to_owned()),
        session_name: Some("work".to_owned()),
        windows: Some(Vec::new()),
        panes: Some(Vec::new()),
        capture: Some(TmuxPaneCapture {
            pane_id: "%1".to_owned(),
            text: "killed capture".to_owned(),
            captured_lines: 1,
            truncated: false,
        }),
        generation: 7,
        ..TmuxDetailState::default()
    };
    view.tmux_current_session_id = Some("$1".to_owned());
    view.tmux_current_session_name = Some("work".to_owned());
    let target = view.tmux_target();
    view.tmux_dialog_context = Some((target, view.tmux_view_generation));
    view.tmux_dialog_session = Some(session);
    view.tmux_pending_kill = Some("$1".to_owned());

    view.kill_tmux_session("$1".to_owned(), cx);
    assert!(view.busy, "kill must be accepted by production gate");
    view.set_tmux_attach_binding(Some(next.clone()), cx);
}

/// 同一 Tab 只换 pane/session，保留 tmux query 目标并让列表与详情继续可读。
///
/// @author kongweiguang
fn binding(
    tab_id: &TabId,
    host_id: Option<&HostId>,
    profile_id: Option<&str>,
    pane_id: PaneId,
) -> TmuxAttachTerminalBinding {
    TmuxAttachTerminalBinding {
        pane_id: pane_id.clone(),
        target: host_id.map_or_else(
            || TmuxAttachTarget::Local {
                profile_id: profile_id.map(str::to_owned),
            },
            |host_id| TmuxAttachTarget::Ssh {
                host_id: host_id.clone(),
            },
        ),
        binding: UiSessionBinding {
            session_id: SessionId::new(),
            owner: SessionOwner::Ui {
                tab_id: tab_id.clone(),
                pane_id,
            },
            session_generation: 4,
        },
        working_directory: None,
    }
}

/// 只构造断言所需的会话字段，避免测试依赖真实 tmux 输出解析。
///
/// @author kongweiguang
fn tmux_session(id: &str, name: &str, target_ref: &str) -> TmuxSession {
    TmuxSession {
        id: id.to_owned(),
        name: name.to_owned(),
        attached: false,
        clients: 0,
        windows: 1,
        current_path: None,
        created_at: None,
        activity_at: None,
        target_ref: target_ref.to_owned(),
    }
}

/// 构造可用能力与单会话快照，用于辨别 setter 是否错误清空列表。
///
/// @author kongweiguang
fn tmux_snapshot(session_id: &str, name: &str, target_ref: &str) -> TmuxSnapshot {
    TmuxSnapshot {
        capability: TmuxCapability {
            target_ref: target_ref.to_owned(),
            available: true,
            version: Some("3.4".to_owned()),
            reason: None,
        },
        sessions: vec![tmux_session(session_id, name, target_ref)],
    }
}

/// 读取已加载 capture 的文本，避免断言时复制整个详情投影。
///
/// @author kongweiguang
fn capture_text(view: &OperationsView) -> Option<&str> {
    view.tmux_details
        .capture
        .as_ref()
        .map(|capture| capture.text.as_str())
}

/// 同 Tab 换 pane 不得清空列表/详情，也不得取消同目标在途读取。
///
/// @author kongweiguang
#[gpui_kit::test]
fn same_tab_pane_switch_on_same_host_preserves_tmux_reads(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let (_runtime, view) = panel(app);
    let tab_id = TabId::new();
    let host_id = HostId::new();
    let previous = binding(&tab_id, Some(&host_id), None, PaneId::new());
    let next = binding(&tab_id, Some(&host_id), None, PaneId::new());
    let target = previous.target.to_tmux_target();
    assert_eq!(target, next.target.to_tmux_target());
    assert_ne!(previous.pane_id, next.pane_id);

    view.update(app, |view, cx| {
        view.active = true;
        view.initialized = true;
        view.tool_rail_target = Some(ToolRailOperationTarget::Ssh {
            host_id: host_id.clone(),
        });
        view.tmux_attach_binding = Some(previous.clone());

        let ready = view.tmux.begin(view.target_generation);
        assert!(
            view.tmux
                .complete(&ready, Ok(tmux_snapshot("$1", "work", "ssh-fixture:work")),)
        );
        let detail_cancellation = CancellationToken::new();
        view.tmux_details = TmuxDetailState {
            session_id: Some("$1".to_owned()),
            session_name: Some("work".to_owned()),
            windows: Some(Vec::new()),
            windows_loading: true,
            panes: Some(Vec::new()),
            panes_loading: true,
            capture: Some(TmuxPaneCapture {
                pane_id: "%1".to_owned(),
                text: "last capture".to_owned(),
                captured_lines: 1,
                truncated: false,
            }),
            generation: 7,
            cancellation: Some(detail_cancellation.clone()),
            ..TmuxDetailState::default()
        };
        view.tmux_detach_pending = Some(TmuxDetachIntent {
            target: target.clone(),
            terminal: previous.clone(),
        });
        view.tmux_dialog_context = Some((target.clone(), view.tmux_view_generation));
        view.tmux_dialog_session = Some(tmux_session("$1", "work", "ssh-fixture:work"));
        view.tmux_pending_kill = Some("$1".to_owned());
        view.tmux_rename_target = Some("$1".to_owned());
        view.tmux_create_form_open = true;
        view.tmux_create_cwd_snapshot = Some(Some("/tmp".to_owned()));
        view.status = Some("old status".to_owned());

        let target_generation = view.target_generation;
        let view_generation = view.tmux_view_generation;
        let details_generation = view.tmux_details.generation;
        let in_flight = view.tmux.begin(view.target_generation);
        let in_flight_cancellation = in_flight.cancellation.clone();
        assert_eq!(view.tmux.phase(), SectionPhase::Loading);
        assert!(view.tmux.is_current(&in_flight));
        assert!(!in_flight_cancellation.is_cancelled());

        view.set_tmux_attach_binding(Some(next.clone()), cx);

        assert_eq!(view.tmux_attach_binding.as_ref(), Some(&next));
        assert_eq!(view.target_generation, target_generation);
        assert_eq!(
            view.tmux_view_generation,
            view_generation.wrapping_add(1).max(1)
        );
        assert_eq!(view.tmux.phase(), SectionPhase::Loading);
        assert_eq!(
            view.tmux
                .data()
                .and_then(|snapshot| snapshot.sessions.first())
                .map(|session| session.id.as_str()),
            Some("$1")
        );
        assert_eq!(view.tmux_details.session_id.as_deref(), Some("$1"));
        assert_eq!(view.tmux_details.generation, details_generation);
        assert_eq!(capture_text(view), Some("last capture"));
        assert!(view.tmux_details.windows_loading);
        assert!(view.tmux_details.panes_loading);
        assert!(!detail_cancellation.is_cancelled());
        assert!(view.tmux_detach_pending.is_none());
        assert!(view.tmux_dialog_context.is_none());
        assert!(view.tmux_dialog_session.is_none());
        assert!(view.tmux_pending_kill.is_none());
        assert!(view.tmux_rename_target.is_none());
        assert!(!view.tmux_create_form_open);
        assert!(view.tmux_create_cwd_snapshot.is_none());
        assert!(view.status.is_none());
        assert!(view.tmux.is_current(&in_flight));
        assert!(!in_flight_cancellation.is_cancelled());

        assert!(view.tmux.complete(
            &in_flight,
            Ok(tmux_snapshot("$2", "next", "ssh-fixture:next")),
        ));
        assert_eq!(view.tmux.phase(), SectionPhase::Ready);
        assert_eq!(
            view.tmux
                .data()
                .and_then(|snapshot| snapshot.sessions.first())
                .map(|session| session.id.as_str()),
            Some("$2")
        );
    });
}

/// 目标身份变化必须清空旧列表/详情、撤销旧确认并拒绝旧目标回执。
///
/// @author kongweiguang
#[gpui_kit::test]
fn target_change_discards_old_tmux_values_and_rejects_stale_receipt(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let (_runtime, view) = panel(app);
    let tab_id = TabId::new();
    let previous = binding(&tab_id, None, Some("alpha"), PaneId::new());
    let next = binding(&tab_id, None, Some("beta"), PaneId::new());
    let previous_target = previous.target.to_tmux_target();
    let next_target = next.target.to_tmux_target();
    assert_ne!(previous_target, next_target);

    view.update(app, |view, cx| {
        view.active = true;
        view.initialized = true;
        view.tool_rail_target = Some(ToolRailOperationTarget::Local);
        view.tmux_attach_binding = Some(previous.clone());

        let old = view.tmux.begin(view.target_generation);
        assert!(
            view.tmux
                .complete(&old, Ok(tmux_snapshot("$old", "old", "local:alpha")),)
        );
        let detail_cancellation = CancellationToken::new();
        view.tmux_details = TmuxDetailState {
            session_id: Some("$old".to_owned()),
            session_name: Some("old".to_owned()),
            windows: Some(Vec::new()),
            windows_loading: true,
            panes: Some(Vec::new()),
            panes_loading: true,
            capture: Some(TmuxPaneCapture {
                pane_id: "%old".to_owned(),
                text: "old capture".to_owned(),
                captured_lines: 1,
                truncated: false,
            }),
            generation: 9,
            cancellation: Some(detail_cancellation.clone()),
            ..TmuxDetailState::default()
        };
        view.tmux_detach_pending = Some(TmuxDetachIntent {
            target: previous_target.clone(),
            terminal: previous.clone(),
        });
        view.tmux_dialog_context = Some((previous_target.clone(), view.tmux_view_generation));
        view.tmux_dialog_session = Some(tmux_session("$old", "old", "local:alpha"));
        view.tmux_pending_kill = Some("$old".to_owned());
        view.tmux_rename_target = Some("$old".to_owned());
        view.tmux_create_form_open = true;
        view.tmux_create_cwd_snapshot = Some(Some("/old".to_owned()));
        view.status = Some("old status".to_owned());

        // 初始快照成功后另起在途请求，避免把已完成票据误当成切换时的旧回执。
        let old_in_flight = view.tmux.begin(view.target_generation);
        let old_in_flight_cancellation = old_in_flight.cancellation.clone();
        assert!(view.tmux.is_current(&old_in_flight));
        assert!(!old_in_flight_cancellation.is_cancelled());

        let target_generation = view.target_generation;
        let view_generation = view.tmux_view_generation;
        let details_generation = view.tmux_details.generation;

        view.set_tmux_attach_binding(Some(next.clone()), cx);

        assert_eq!(view.tmux_attach_binding.as_ref(), Some(&next));
        assert_eq!(
            view.target_generation,
            target_generation.wrapping_add(1).max(1)
        );
        assert_eq!(
            view.tmux_view_generation,
            view_generation.wrapping_add(1).max(1)
        );
        assert_eq!(view.tmux.phase(), SectionPhase::Loading);
        assert!(view.tmux.data().is_none());
        assert!(view.tmux_details.session_id.is_none());
        assert!(view.tmux_details.session_name.is_none());
        assert!(view.tmux_details.windows.is_none());
        assert!(view.tmux_details.panes.is_none());
        assert!(view.tmux_details.capture.is_none());
        // 取消详情在途读取本身递增一次代次，随后目标重置再递增一次。
        assert_eq!(
            view.tmux_details.generation,
            details_generation.wrapping_add(2).max(1)
        );
        assert!(detail_cancellation.is_cancelled());
        assert!(view.tmux_detach_pending.is_none());
        assert!(view.tmux_dialog_context.is_none());
        assert!(view.tmux_dialog_session.is_none());
        assert!(view.tmux_pending_kill.is_none());
        assert!(view.tmux_rename_target.is_none());
        assert!(!view.tmux_create_form_open);
        assert!(view.tmux_create_cwd_snapshot.is_none());
        assert!(view.status.is_none());

        assert!(!view.tmux.is_current(&old_in_flight));
        assert!(old_in_flight_cancellation.is_cancelled());
        assert!(!view.tmux.complete(
            &old_in_flight,
            Ok(tmux_snapshot("$stale", "stale", "local:alpha")),
        ));
        assert!(view.tmux.data().is_none());
        assert_eq!(view.tmux.phase(), SectionPhase::Loading);
    });
}

/// Root 先切 SSH host 再绑定新 pane 时，旧 host 的列表/详情必须在第一步被丢弃，
/// 第二步只更新 owner，不能复活旧值或接受旧 host 的迟到回执。
///
/// @author kongweiguang
#[gpui_kit::test]
fn ssh_host_switch_then_binding_uses_production_order(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let (_runtime, view) = panel(app);
    let tab_id = TabId::new();
    let old_host = HostId::new();
    let new_host = HostId::new();
    let previous = binding(&tab_id, Some(&old_host), None, PaneId::new());
    let next = binding(&tab_id, Some(&new_host), None, PaneId::new());

    view.update(app, |view, cx| {
        // 生产 Root 先更新宿主目标；测试在未激活时布置旧 host 数据，避免布置阶段自行读取。
        view.active = false;
        view.initialized = false;
        view.set_tool_rail_target(
            ToolRailOperationTarget::Ssh {
                host_id: old_host.clone(),
            },
            cx,
        );
        view.tmux_attach_binding = Some(previous.clone());

        let old = view.tmux.begin(view.target_generation);
        assert!(
            view.tmux
                .complete(&old, Ok(tmux_snapshot("$old", "old", "ssh-old:old")),)
        );
        let detail_cancellation = CancellationToken::new();
        view.tmux_details = TmuxDetailState {
            session_id: Some("$old".to_owned()),
            session_name: Some("old".to_owned()),
            windows: Some(Vec::new()),
            windows_loading: true,
            panes: Some(Vec::new()),
            panes_loading: true,
            capture: Some(TmuxPaneCapture {
                pane_id: "%old".to_owned(),
                text: "old capture".to_owned(),
                captured_lines: 1,
                truncated: false,
            }),
            generation: 11,
            cancellation: Some(detail_cancellation.clone()),
            ..TmuxDetailState::default()
        };
        let previous_target = previous.target.to_tmux_target();
        view.tmux_detach_pending = Some(TmuxDetachIntent {
            target: previous_target.clone(),
            terminal: previous.clone(),
        });
        view.tmux_dialog_context = Some((previous_target, view.tmux_view_generation));
        view.tmux_dialog_session = Some(tmux_session("$old", "old", "ssh-old:old"));
        view.tmux_pending_kill = Some("$old".to_owned());
        view.tmux_rename_target = Some("$old".to_owned());
        view.tmux_create_form_open = true;
        view.tmux_create_cwd_snapshot = Some(Some("/old".to_owned()));
        view.status = Some("old status".to_owned());
        let old_in_flight = view.tmux.begin(view.target_generation);
        let old_in_flight_cancellation = old_in_flight.cancellation.clone();
        assert!(view.tmux.is_current(&old_in_flight));
        assert!(!old_in_flight_cancellation.is_cancelled());

        view.active = true;
        view.initialized = true;
        let target_generation = view.target_generation;
        let view_generation = view.tmux_view_generation;
        let details_generation = view.tmux_details.generation;

        // 第一步：Root 先切换 SSH host。目标代次重置后旧列表中不再属于当前目标。
        view.set_tool_rail_target(
            ToolRailOperationTarget::Ssh {
                host_id: new_host.clone(),
            },
            cx,
        );

        assert_eq!(
            view.tool_rail_target,
            Some(ToolRailOperationTarget::Ssh {
                host_id: new_host.clone(),
            })
        );
        assert_eq!(
            view.target_generation,
            target_generation.wrapping_add(1).max(1)
        );
        assert_eq!(
            view.tmux_view_generation,
            view_generation.wrapping_add(1).max(1)
        );
        assert_eq!(view.tmux.phase(), SectionPhase::Loading);
        assert!(view.tmux.data().is_none());
        assert!(view.tmux_details.session_id.is_none());
        assert!(view.tmux_details.capture.is_none());
        // 取消详情在途读取本身递增一次代次，随后目标重置再递增一次。
        assert_eq!(
            view.tmux_details.generation,
            details_generation.wrapping_add(2).max(1)
        );
        assert!(detail_cancellation.is_cancelled());
        assert!(view.tmux_detach_pending.is_none());
        assert!(view.tmux_dialog_context.is_none());
        assert!(view.status.is_none());
        assert!(!view.tmux.is_current(&old_in_flight));
        assert!(old_in_flight_cancellation.is_cancelled());
        assert!(!view.tmux.complete(
            &old_in_flight,
            Ok(tmux_snapshot("$old", "old", "ssh-old:old")),
        ));
        assert!(view.tmux.data().is_none());
        // 第一步已启动新 host 的读取；记录其请求序号，第二步不得重启这条在途读取。
        let in_flight_request_id = view.tmux.acceptance_request_id();

        let target_generation = view.target_generation;
        let view_generation = view.tmux_view_generation;
        let details_generation = view.tmux_details.generation;

        // 第二步：Root 再绑定新 host 的 pane。查询目标已在第一步切换，这里只能更新 owner，
        // 既不能重置第一步启动的新 host 读取，也不能复活旧 host 数据。
        view.set_tmux_attach_binding(Some(next.clone()), cx);

        assert_eq!(view.tmux_attach_binding.as_ref(), Some(&next));
        assert_eq!(view.target_generation, target_generation);
        assert_eq!(
            view.tmux_view_generation,
            view_generation.wrapping_add(1).max(1)
        );
        assert_eq!(view.tmux.phase(), SectionPhase::Loading);
        assert!(view.tmux.data().is_none());
        assert!(view.tmux_details.session_id.is_none());
        assert!(view.tmux_details.capture.is_none());
        assert_eq!(view.tmux_details.generation, details_generation);
        assert!(view.tmux_detach_pending.is_none());
        assert!(view.tmux_dialog_context.is_none());
        assert!(view.status.is_none());
        // request_id 未变，证明第二步没有 reset/begin，新 host 的在途读取仍属同一次请求。
        assert_eq!(view.tmux.acceptance_request_id(), in_flight_request_id);
    });
}

/// 真实 `load_tmux` 经过 probe 后若能力不可用，没有执行 list，不得消费写后 dirty。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_probe_only_unavailable_keeps_dirty_until_real_list(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Unavailable);
    let (runtime, view, _tab, _attach) = prepared_local_panel(app, script.clone(), None);
    view.update(app, |view, cx| {
        let dirty_target = view.tmux_target();
        view.dirty_targets.mark_tmux_dirty(dirty_target);
        view.load_tmux(cx);
    });
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Ready
    });

    assert_eq!(script.probe_calls(), 1);
    assert_eq!(script.list_calls(), 0);
    view.update(app, |view, _| {
        assert!(
            !view
                .tmux
                .data()
                .expect("probe snapshot")
                .capability
                .available
        );
        assert!(view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
    });
}

/// probe 端口失败进入 Failed，没有权威列表回执，dirty 保留供恢复后重新核对。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_probe_transport_error_keeps_dirty(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Error);
    let (runtime, view, _tab, _attach) = prepared_local_panel(app, script.clone(), None);
    view.update(app, |view, cx| {
        let dirty_target = view.tmux_target();
        view.dirty_targets.mark_tmux_dirty(dirty_target);
        view.load_tmux(cx);
    });
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Failed
    });

    assert_eq!(script.probe_calls(), 1);
    assert_eq!(script.list_calls(), 0);
    view.update(app, |view, _| {
        assert!(view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
    });
}

/// 真实成功空列表是权威读取，才允许清除该目标的 dirty；失败与取消不代办。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_real_empty_list_clears_dirty_but_cancel_keeps_it(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_list_replies([ports::TmuxListReply::Success]);
    let (runtime, view, _tab, _attach) = prepared_local_panel(app, script.clone(), None);
    view.update(app, |view, cx| {
        let dirty_target = view.tmux_target();
        view.dirty_targets.mark_tmux_dirty(dirty_target);
        view.load_tmux(cx);
    });
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Ready
    });
    assert_eq!(script.list_calls(), 1);
    view.update(app, |view, _| {
        assert!(
            view.tmux
                .data()
                .expect("empty list snapshot")
                .sessions
                .is_empty()
        );
        assert!(!view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
    });

    view.update(app, |view, cx| {
        let dirty_target = view.tmux_target();
        view.dirty_targets.mark_tmux_dirty(dirty_target);
        view.load_tmux(cx);
        view.tmux.cancel_pending();
        assert_eq!(view.tmux.phase(), SectionPhase::Cancelled);
    });
    pump(app, &runtime);
    view.update(app, |view, _| {
        assert_eq!(view.tmux.phase(), SectionPhase::Cancelled);
        assert!(view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
    });
}

/// probe-only 不可用没有执行 `list_sessions`，空快照不能当作“已选会话消失”的清空依据；
/// 真实成功空列表才是权威读取，才允许清理旧详情并消费入场。
///
/// 确认弹窗在无法核对身份时按安全侧关闭并提示重新选择，但不得据此声称会话已删除。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_probe_only_unavailable_keeps_selected_details_until_real_list_confirms_absence(
    app: &mut TestAppContext,
) {
    app.update(gpui_kit::init);
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_probe_replies([
            ports::TmuxProbeReply::Available,
            ports::TmuxProbeReply::Unavailable,
        ])
        .with_list_replies([ports::TmuxListReply::Success, ports::TmuxListReply::Success]);
    let (runtime, view, _tab, _attach) = prepared_local_panel(app, script.clone(), None);

    // 先建立真实可用空列表基线，再布置仍可能有效的已选详情与待确认弹窗。
    view.update(app, |view, cx| view.load_tmux(cx));
    pump_until(app, &runtime, &view, |view| {
        view.tmux
            .data()
            .is_some_and(|snapshot| snapshot.capability.available)
    });
    assert_eq!(script.list_calls(), 1);
    view.update(app, |view, cx| {
        view.tmux_details = TmuxDetailState {
            session_id: Some("$1".to_owned()),
            session_name: Some("work".to_owned()),
            windows: Some(Vec::new()),
            panes: Some(Vec::new()),
            capture: Some(TmuxPaneCapture {
                pane_id: "%1".to_owned(),
                text: "surviving capture".to_owned(),
                captured_lines: 1,
                truncated: false,
            }),
            generation: 7,
            ..TmuxDetailState::default()
        };
        view.tmux_dialog_session = Some(tmux_session_with_identity("$1", "work", "local", 100));
        view.arm_fixed_data_entrance(OperationsSection::Tmux, true, cx);
    });

    // 同目标刷新只探测到 tmux 不可用：没有列表结果，不能清详情或消费入场。
    view.update(app, |view, cx| view.load_tmux(cx));
    pump_until(app, &runtime, &view, |view| {
        view.tmux
            .data()
            .is_some_and(|snapshot| !snapshot.capability.available)
    });
    assert_eq!(script.probe_calls(), 2);
    assert_eq!(
        script.list_calls(),
        1,
        "probe-only must not read the session list"
    );
    view.update(app, |view, _| {
        assert_eq!(view.tmux_details.session_id.as_deref(), Some("$1"));
        assert_eq!(capture_text(view), Some("surviving capture"));
        assert_eq!(view.tmux_details.generation, 7);
        assert!(view.tmux_data_entrance.awaiting_result());
        // 无法核对身份时按安全侧关闭弹窗；提示覆盖“无法确认”，不伪装成已删除。
        assert!(view.tmux_dialog_session.is_none());
        assert!(view.status.as_deref().is_some_and(|status| {
            status.contains("无法确认原身份") || status.contains("could not be verified")
        }));
    });

    // 恢复后的真实成功空列表才确认已选会话消失，这时才清理详情并消费入场。
    view.update(app, |view, cx| view.load_tmux(cx));
    pump_until(app, &runtime, &view, |view| {
        view.tmux
            .data()
            .is_some_and(|snapshot| snapshot.capability.available)
    });
    assert_eq!(script.list_calls(), 2);
    view.update(app, |view, _| {
        assert!(view.tmux_details.session_id.is_none());
        assert!(view.tmux_details.capture.is_none());
        assert!(!view.tmux_data_entrance.awaiting_result());
    });
}

/// 旧查询目标的成功回执在目标代次变化后必须被拒绝，不能清除新目标的 dirty。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_stale_target_receipt_cannot_clear_new_target_dirty(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let host_a = HostId::new();
    let host_b = HostId::new();
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_host_list_reply(host_a.clone(), ports::TmuxListReply::Success)
        .with_host_list_reply(host_b.clone(), ports::TmuxListReply::Error);
    script.hold_next_list();
    let (runtime, view) = panel_with_commands(app, script.clone());
    let tab_id = TabId::new();
    let previous = binding(&tab_id, Some(&host_a), None, PaneId::new());
    view.update(app, |view, _| {
        view.active = true;
        view.initialized = true;
        view.tool_rail_target = Some(ToolRailOperationTarget::Ssh {
            host_id: host_a.clone(),
        });
        view.tmux_attach_binding = Some(previous.clone());
    });
    view.update(app, |view, cx| view.load_tmux(cx));
    pump(app, &runtime);
    assert!(script.list_waiting(), "old target list must be in flight");

    view.update(app, |view, cx| {
        view.set_tool_rail_target(
            ToolRailOperationTarget::Ssh {
                host_id: host_b.clone(),
            },
            cx,
        );
    });
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Failed
    });
    assert_eq!(script.list_calls(), 2);

    // 新目标已经失败；此时释放旧目标成功空列表，旧代次回执只能被拒绝。
    script.release_list();
    pump(app, &runtime);
    view.update(app, |view, _| {
        assert_eq!(view.tmux.phase(), SectionPhase::Failed);
        assert!(view.tmux.data().is_none());
        assert!(view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
    });
}

/// 同一目标的旧刷新票据即使命令成功，也不能在新请求失败后代为清除 dirty。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_stale_same_target_ticket_cannot_clear_dirty(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_list_replies([ports::TmuxListReply::Success, ports::TmuxListReply::Error]);
    script.hold_next_list();
    let (runtime, view, _tab, _attach) = prepared_local_panel(app, script.clone(), None);
    view.update(app, |view, cx| {
        let dirty_target = view.tmux_target();
        view.dirty_targets.mark_tmux_dirty(dirty_target);
        view.load_tmux(cx);
    });
    pump(app, &runtime);
    assert!(script.list_waiting(), "old ticket list must be in flight");

    view.update(app, |view, cx| view.load_tmux(cx));
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Failed
    });
    assert_eq!(script.list_calls(), 2);

    script.release_list();
    pump(app, &runtime);
    view.update(app, |view, _| {
        assert_eq!(view.tmux.phase(), SectionPhase::Failed);
        assert!(view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
    });
}

/// A→B→A 的固定 tmux 目标重置会重新置未确认；失败后激活仍重试，成功才清 dirty 和入场。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_fixed_round_trip_rearms_bounded_dirty_and_data_entrance(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let host_a = HostId::new();
    let host_b = HostId::new();
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_host_list_sequence(
            host_a.clone(),
            [
                ports::TmuxListReply::Success,
                ports::TmuxListReply::Error,
                ports::TmuxListReply::Success,
            ],
        )
        .with_host_list_reply(host_b.clone(), ports::TmuxListReply::Error);
    let (runtime, view) = panel_with_commands(app, script.clone());
    let tab_id = TabId::new();
    let attach_a = binding(&tab_id, Some(&host_a), None, PaneId::new());
    let attach_b = binding(&tab_id, Some(&host_b), None, PaneId::new());

    view.update(app, |view, cx| {
        view.active = true;
        view.initialized = true;
        view.set_tool_rail_target(
            ToolRailOperationTarget::Ssh {
                host_id: host_a.clone(),
            },
            cx,
        );
        view.set_tmux_attach_binding(Some(attach_a.clone()), cx);
    });
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Ready
    });
    assert_eq!(script.list_calls(), 1);
    view.update(app, |view, _| {
        assert!(!view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
        assert!(!view.tmux_data_entrance.awaiting_result());
    });

    // 同目标换 pane 不重置目标，也不产生新的 dirty 或入场等待。
    view.update(app, |view, cx| {
        let epoch = view.tmux_data_entrance.epoch();
        view.set_tmux_attach_binding(
            Some(binding(&tab_id, Some(&host_a), None, PaneId::new())),
            cx,
        );
        assert_eq!(view.tmux_data_entrance.epoch(), epoch);
    });
    assert_eq!(script.list_calls(), 1);
    view.update(app, |view, _| {
        assert!(!view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
        assert!(!view.tmux_data_entrance.awaiting_result());
    });

    // B 的首次读取失败：B 由目标重置天然未确认，不依赖 A 的历史 dirty。
    view.update(app, |view, cx| {
        view.set_tool_rail_target(
            ToolRailOperationTarget::Ssh {
                host_id: host_b.clone(),
            },
            cx,
        );
        let epoch = view.tmux_data_entrance.epoch();
        assert!(view.tmux_data_entrance.awaiting_result());
        view.set_tmux_attach_binding(Some(attach_b), cx);
        // 生产顺序已经由目标 setter 重置并 arm；绑定同一新目标不能再 arm 一次。
        assert_eq!(view.tmux_data_entrance.epoch(), epoch);
    });
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Failed
    });
    assert_eq!(script.list_calls(), 2);
    view.update(app, |view, _| {
        assert!(view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
        assert!(view.tmux_data_entrance.awaiting_result());
    });

    // 返回 A：即使离开期间 A 没有写回，目标重置仍先置未确认；首次读取失败保留它。
    view.update(app, |view, cx| {
        view.set_tool_rail_target(
            ToolRailOperationTarget::Ssh {
                host_id: host_a.clone(),
            },
            cx,
        );
        let epoch = view.tmux_data_entrance.epoch();
        assert!(view.tmux_data_entrance.awaiting_result());
        view.set_tmux_attach_binding(Some(attach_a), cx);
        assert_eq!(view.tmux_data_entrance.epoch(), epoch);
    });
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Failed
    });
    assert_eq!(script.list_calls(), 3);
    view.update(app, |view, _| {
        assert!(view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
        assert!(view.tmux_data_entrance.awaiting_result());
    });

    // 隐藏再激活必须重试当前目标；只有成功列表同时消费 dirty 与待播入场。
    view.update(app, |view, cx| view.set_active(false, cx));
    view.update(app, |view, cx| view.set_active(true, cx));
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Ready
    });
    assert_eq!(script.list_calls(), 4);
    view.update(app, |view, _| {
        assert!(!view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
        assert!(!view.tmux_data_entrance.awaiting_result());
    });
}

/// 同一默认 Local 从目标不可用恢复时也要登记一次入场；身份未变，只有可用性变化。
///
/// 首次真实读取失败会保留等待，成功才消费；同目标普通换 pane 不得再次登记。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_local_availability_recovery_arms_once_for_same_default_target(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_list_replies([ports::TmuxListReply::Success, ports::TmuxListReply::Success]);
    let (runtime, view, tab_id, _attach) = prepared_local_panel(app, script.clone(), None);

    view.update(app, |view, cx| view.load_tmux(cx));
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Ready
    });
    assert_eq!(script.list_calls(), 1);
    view.update(app, |view, _| {
        assert!(!view.tmux_data_entrance.awaiting_result());
    });

    // pane 消失后查询目标仍是同一默认 Local，但进入不可用并重置读取。
    view.update(app, |view, cx| view.set_tmux_attach_binding(None, cx));
    view.update(app, |view, _| {
        assert!(!view.tmux_target_available());
        assert!(!view.tmux_data_entrance.awaiting_result());
    });

    // 同一默认 Local 恢复：profile 仍为 None，只有可用性变化，仍要等待首个真实结果。
    view.update(app, |view, cx| {
        view.set_tmux_attach_binding(Some(binding(&tab_id, None, None, PaneId::new())), cx);
        assert!(view.tmux_data_entrance.awaiting_result());
        let epoch = view.tmux_data_entrance.epoch();
        // 同一目标的普通换 pane 只更新 owner，不重复登记入场或发起读取。
        view.set_tmux_attach_binding(Some(binding(&tab_id, None, None, PaneId::new())), cx);
        assert_eq!(view.tmux_data_entrance.epoch(), epoch);
    });
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Ready && !view.tmux_data_entrance.awaiting_result()
    });
    assert_eq!(script.list_calls(), 2);
    view.update(app, |view, _| {
        assert!(!view.tmux_data_entrance.awaiting_result());
        assert!(!view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
    });
}

/// Local profile 切换走 attach_binding 目标重置：旧详情清空，新目标 dirty 与入场
/// 需要同目标真实成功列表才消费；列表失败与 probe-only 不可用都不能伪报确认。
///
/// 两个 profile 目标都是真实登记的本地主机 ID，且经生产 `local_session_target`
/// 解析；这里不能改用空 profile，否则会绕开本机主机解析路径而丢掉测试意图。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_local_profile_switch_requires_current_target_success(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_probe_replies([
            ports::TmuxProbeReply::Available,
            ports::TmuxProbeReply::Available,
            ports::TmuxProbeReply::Unavailable,
            ports::TmuxProbeReply::Available,
        ])
        .with_list_replies([
            ports::TmuxListReply::Success,
            ports::TmuxListReply::Error,
            ports::TmuxListReply::Success,
        ]);
    let alpha = ports::local_host_fixture("pwsh", Some("C:/alpha"));
    let beta = ports::local_host_fixture("pwsh", Some("C:/beta"));
    let alpha_id = alpha.id.as_str().to_owned();
    let beta_id = beta.id.as_str().to_owned();
    assert_ne!(alpha_id, beta_id, "profiles must be distinct local hosts");
    let (runtime, view, tab_id, _attach) = prepared_local_panel_with_service(
        app,
        script.clone(),
        Some(alpha_id.as_str()),
        Some(vec![alpha, beta]),
    );

    view.update(app, |view, cx| view.load_tmux(cx));
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Ready
    });
    assert_eq!(script.list_calls(), 1);
    view.update(app, |view, _| {
        view.tmux_details = TmuxDetailState {
            session_id: Some("$alpha".to_owned()),
            session_name: Some("alpha work".to_owned()),
            capture: Some(TmuxPaneCapture {
                pane_id: "%alpha".to_owned(),
                text: "alpha capture".to_owned(),
                captured_lines: 1,
                truncated: false,
            }),
            generation: 5,
            ..TmuxDetailState::default()
        };
    });

    // alpha → beta 是真实目标变化：立即丢弃旧详情、置 dirty 并登记入场。
    view.update(app, |view, cx| {
        view.set_tmux_attach_binding(
            Some(binding(
                &tab_id,
                None,
                Some(beta_id.as_str()),
                PaneId::new(),
            )),
            cx,
        );
        assert_eq!(
            view.tmux_target().profile_id.as_deref(),
            Some(beta_id.as_str())
        );
        assert!(view.tmux_details.session_id.is_none());
        assert!(view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
        assert!(view.tmux_data_entrance.awaiting_result());
    });
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Failed
    });
    assert_eq!(script.list_calls(), 2);
    view.update(app, |view, _| {
        assert!(view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
        assert!(view.tmux_data_entrance.awaiting_result());
    });

    // 隐藏再激活先只探测到不可用：没有列表结果，不能清 dirty 或消费入场。
    view.update(app, |view, cx| view.set_active(false, cx));
    view.update(app, |view, cx| view.set_active(true, cx));
    pump_until(app, &runtime, &view, |view| {
        view.tmux
            .data()
            .is_some_and(|snapshot| !snapshot.capability.available)
    });
    assert_eq!(script.list_calls(), 2);
    view.update(app, |view, _| {
        assert!(view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
        assert!(view.tmux_data_entrance.awaiting_result());
    });

    // 再次激活读到同目标真实成功列表，才清除 dirty、消费入场。
    view.update(app, |view, cx| view.set_active(false, cx));
    view.update(app, |view, cx| view.set_active(true, cx));
    pump_until(app, &runtime, &view, |view| {
        view.tmux
            .data()
            .is_some_and(|snapshot| snapshot.capability.available)
            && !view.tmux_data_entrance.awaiting_result()
    });
    assert_eq!(script.list_calls(), 3);
    view.update(app, |view, _| {
        assert!(!view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
        assert_eq!(
            view.tmux_target().profile_id.as_deref(),
            Some(beta_id.as_str())
        );
    });
}

/// probe-only 不可用不算成功列表：返回经过 A→B→A 的目标仍保留 dirty 与入场等待。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_round_trip_unavailable_keeps_bounded_dirty_until_recovery(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let host_a = HostId::new();
    let host_b = HostId::new();
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_probe_replies([
            ports::TmuxProbeReply::Available,
            ports::TmuxProbeReply::Available,
            ports::TmuxProbeReply::Unavailable,
            ports::TmuxProbeReply::Available,
        ])
        .with_host_list_sequence(
            host_a.clone(),
            [ports::TmuxListReply::Success, ports::TmuxListReply::Success],
        )
        .with_host_list_reply(host_b.clone(), ports::TmuxListReply::Success);
    let (runtime, view) = panel_with_commands(app, script.clone());
    let tab_id = TabId::new();
    let attach_a = binding(&tab_id, Some(&host_a), None, PaneId::new());
    let attach_b = binding(&tab_id, Some(&host_b), None, PaneId::new());

    view.update(app, |view, cx| {
        view.active = true;
        view.initialized = true;
        view.set_tool_rail_target(
            ToolRailOperationTarget::Ssh {
                host_id: host_a.clone(),
            },
            cx,
        );
        view.set_tmux_attach_binding(Some(attach_a.clone()), cx);
    });
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Ready
    });
    assert_eq!(script.list_calls(), 1);

    view.update(app, |view, cx| {
        view.set_tool_rail_target(
            ToolRailOperationTarget::Ssh {
                host_id: host_b.clone(),
            },
            cx,
        );
        view.set_tmux_attach_binding(Some(attach_b), cx);
    });
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Ready
    });
    assert_eq!(script.list_calls(), 2);

    // 返回 A 时只探测到 tmux 不可用，没有真实列表成功，不能消费 dirty 或入场机会。
    view.update(app, |view, cx| {
        view.set_tool_rail_target(
            ToolRailOperationTarget::Ssh {
                host_id: host_a.clone(),
            },
            cx,
        );
        view.set_tmux_attach_binding(Some(attach_a), cx);
    });
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Ready
    });
    assert_eq!(script.list_calls(), 2);
    view.update(app, |view, _| {
        assert!(
            !view
                .tmux
                .data()
                .expect("unavailable probe snapshot")
                .capability
                .available
        );
        assert!(view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
        assert!(view.tmux_data_entrance.awaiting_result());
    });

    view.update(app, |view, cx| view.set_active(false, cx));
    view.update(app, |view, cx| view.set_active(true, cx));
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Ready
    });
    assert_eq!(script.list_calls(), 3);
    view.update(app, |view, _| {
        assert!(
            view.tmux
                .data()
                .expect("recovered snapshot")
                .capability
                .available
        );
        assert!(!view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
        assert!(!view.tmux_data_entrance.awaiting_result());
    });
}

/// 中央运维页同样在每次目标重置时置未确认，仅当前目标成功列表清除，失败保留。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_dashboard_target_resets_scope_dirty_to_current_snapshot(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let host_a = HostId::new();
    let host_b = HostId::new();
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_host_list_sequence(
            host_a.clone(),
            [ports::TmuxListReply::Success, ports::TmuxListReply::Success],
        )
        .with_host_list_reply(host_b.clone(), ports::TmuxListReply::Error);
    let (runtime, view) = dashboard_with_commands(app, script.clone());

    view.update(app, |view, cx| {
        view.active = false;
        view.initialized = true;
        view.set_host_target(Some(host_a.clone()), cx);
    });
    view.update(app, |view, _| {
        assert!(view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
    });
    view.update(app, |view, cx| view.load_tmux(cx));
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Ready
    });
    assert_eq!(script.list_calls(), 1);
    view.update(app, |view, _| {
        assert!(!view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
    });

    view.update(app, |view, cx| {
        view.set_host_target(Some(host_b), cx);
    });
    view.update(app, |view, cx| view.load_tmux(cx));
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Failed
    });
    assert_eq!(script.list_calls(), 2);
    view.update(app, |view, _| {
        assert!(view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
    });

    view.update(app, |view, cx| {
        view.set_host_target(Some(host_a), cx);
    });
    view.update(app, |view, cx| view.load_tmux(cx));
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Ready
    });
    assert_eq!(script.list_calls(), 3);
    view.update(app, |view, _| {
        assert!(!view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
    });
}

/// 同目标换 pane 后 kill 成功、权威刷新失败时，只清理经冻结身份门禁确认的旧详情。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_confirmed_kill_after_pane_switch_clears_only_old_details(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_list_replies([ports::TmuxListReply::Error]);
    let (runtime, view, tab_id, _attach) = prepared_local_panel(app, script.clone(), None);
    let next = binding(&tab_id, None, None, PaneId::new());

    view.update(app, |view, cx| {
        seed_kill_and_switch_pane(view, "local", &next, cx);
        assert_eq!(view.tmux_details.session_id.as_deref(), Some("$1"));
        assert_eq!(capture_text(view), Some("killed capture"));
    });
    pump_until(app, &runtime, &view, |view| {
        !view.busy && view.tmux.phase() == SectionPhase::Failed
    });

    assert_eq!(script.kill_calls(), 1);
    assert_eq!(script.list_calls(), 1);
    view.update(app, |view, _| {
        assert!(view.tmux_details.session_id.is_none());
        assert!(view.tmux_details.capture.is_none());
        // 同目标换 pane 只抑制视图代次级回写，但写回执必须让新焦点看见真实结果。
        // 测试不预设应用 locale，断言必须接受双语文案。
        assert!(
            view.status
                .as_deref()
                .is_some_and(|status| status.contains("已结束") || status.contains("session ended"))
        );
        assert_eq!(view.tmux_current_session_id.as_deref(), Some("$1"));
        assert_eq!(view.tmux_current_session_name.as_deref(), Some("work"));
    });
}

/// kill 成功但换 pane 后用户已重选其它会话时，旧回执不得清理新选择或写新 pane 状态。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_successful_kill_does_not_clear_newly_selected_session(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_list_replies([ports::TmuxListReply::Error]);
    let (runtime, view, tab_id, _attach) = prepared_local_panel(app, script.clone(), None);
    let next = binding(&tab_id, None, None, PaneId::new());

    view.update(app, |view, cx| {
        seed_kill_and_switch_pane(view, "local", &next, cx);
        view.select_tmux_session("$2".to_owned(), "other".to_owned(), cx);
        assert_eq!(view.tmux_details.session_id.as_deref(), Some("$2"));
    });
    pump_until(app, &runtime, &view, |view| {
        !view.busy
            && view.tmux.phase() == SectionPhase::Failed
            && view.tmux_details.windows_error.is_some()
    });

    assert_eq!(script.kill_calls(), 1);
    assert_eq!(script.list_calls(), 1);
    view.update(app, |view, _| {
        assert_eq!(view.tmux_details.session_id.as_deref(), Some("$2"));
        assert_eq!(view.tmux_details.session_name.as_deref(), Some("other"));
        assert!(view.tmux_details.windows.is_none());
        assert!(view.tmux_details.capture.is_none());
        // 用户重选会话不改变写回执归属：kill 结果仍属于当前目标并可见。
        assert!(
            view.status
                .as_deref()
                .is_some_and(|status| status.contains("已结束") || status.contains("session ended"))
        );
    });
}

/// 旧目标 kill 迟到回执不缓存历史 dirty；切回旧目标时由目标重置重新置为未确认。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_late_kill_does_not_cache_old_target_but_return_marks_dirty(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let host_a = HostId::new();
    let host_b = HostId::new();
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_host_list_reply(host_b.clone(), ports::TmuxListReply::Success);
    let (runtime, view) = panel_with_commands(app, script.clone());
    let tab_id = TabId::new();
    let previous = binding(&tab_id, Some(&host_a), None, PaneId::new());
    let old_target = previous.target.to_tmux_target();
    view.update(app, |view, cx| {
        view.active = true;
        view.initialized = true;
        view.tool_rail_target = Some(ToolRailOperationTarget::Ssh {
            host_id: host_a.clone(),
        });
        view.tmux_attach_binding = Some(previous.clone());
        let session = tmux_session_with_identity("$1", "work", "ssh-a", 100);
        let ready = view.tmux.begin(view.target_generation);
        assert!(view.tmux.complete(
            &ready,
            Ok(TmuxSnapshot {
                capability: TmuxCapability {
                    target_ref: "ssh-a".to_owned(),
                    available: true,
                    version: Some("tmux 3.4".to_owned()),
                    reason: None,
                },
                sessions: vec![session.clone()],
            }),
        ));
        view.tmux_dialog_context = Some((view.tmux_target(), view.tmux_view_generation));
        view.tmux_dialog_session = Some(session);
        view.tmux_pending_kill = Some("$1".to_owned());

        view.kill_tmux_session("$1".to_owned(), cx);
        // 生产 Root 在写回前切换目标：旧回执仍属于 A，新目标 B 先发起自己的权威读取。
        view.set_tool_rail_target(
            ToolRailOperationTarget::Ssh {
                host_id: host_b.clone(),
            },
            cx,
        );
        view.set_tmux_attach_binding(
            Some(binding(&tab_id, Some(&host_b), None, PaneId::new())),
            cx,
        );
    });
    pump_until(app, &runtime, &view, |view| {
        !view.busy && view.tmux.phase() == SectionPhase::Ready
    });

    assert_eq!(script.kill_calls(), 1);
    assert_eq!(script.list_calls(), 1);
    view.update(app, |view, _| {
        assert_eq!(
            view.tmux_target().host_id.as_ref(),
            Some(&host_b),
            "current query must stay on the new host"
        );
        // 有界状态不保留已离开目标的写回；B 的成功列表只清 B。
        assert!(!view.dirty_targets.tmux_is_dirty(&old_target));
        assert!(!view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
        assert!(
            view.tmux
                .data()
                .expect("new host snapshot")
                .sessions
                .is_empty()
        );
    });

    // 返回 A 是新的目标重置：即使首次读取失败，A 也保留未确认与后续激活重试机会。
    view.update(app, |view, cx| {
        view.set_tool_rail_target(
            ToolRailOperationTarget::Ssh {
                host_id: host_a.clone(),
            },
            cx,
        );
        view.set_tmux_attach_binding(Some(previous.clone()), cx);
    });
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Failed
    });
    assert_eq!(script.list_calls(), 2);
    view.update(app, |view, _| {
        assert_eq!(view.tmux_target().host_id.as_ref(), Some(&host_a));
        assert!(view.dirty_targets.tmux_is_dirty(&view.tmux_target()));
    });
}

/// kill 回执失败时即使同目标换 pane 也不清理旧详情，保留真实失败后的核对材料。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_failed_kill_keeps_old_details_for_retry(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_list_replies([ports::TmuxListReply::Error])
        .with_kill_reply(ports::TmuxKillReply::Error);
    let (runtime, view, tab_id, _attach) = prepared_local_panel(app, script.clone(), None);
    let next = binding(&tab_id, None, None, PaneId::new());

    view.update(app, |view, cx| {
        seed_kill_and_switch_pane(view, "local", &next, cx);
    });
    pump_until(app, &runtime, &view, |view| {
        !view.busy && view.tmux.phase() == SectionPhase::Failed
    });

    assert_eq!(script.kill_calls(), 1);
    assert_eq!(script.list_calls(), 1);
    view.update(app, |view, _| {
        assert_eq!(view.tmux_details.session_id.as_deref(), Some("$1"));
        assert_eq!(capture_text(view), Some("killed capture"));
        // 失败结果不能因为换 pane 被视图代次门禁吞掉，必须让新焦点看到原因。
        assert!(
            view.status
                .as_deref()
                .is_some_and(|status| status.contains("fixture tmux kill failed"))
        );
    });
}

/// detach 失败必须归属当前目标：同目标换 pane 后新焦点立即可见，不被代次门禁吞掉。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_detach_failure_after_pane_switch_is_visible_immediately(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_list_replies([ports::TmuxListReply::Success])
        .with_detach_reply(ports::TmuxKillReply::Error);
    let (runtime, view, tab_id, _attach) = prepared_local_panel(app, script.clone(), None);

    // 同目标换 pane 后 detach 失败：新焦点必须立即看到失败，而不是被代次门禁吞掉。
    view.update(app, |view, cx| {
        view.detach_tmux_session("$1".to_owned(), cx);
        assert!(view.busy);
        view.set_tmux_attach_binding(Some(binding(&tab_id, None, None, PaneId::new())), cx);
    });
    pump_until(app, &runtime, &view, |view| !view.busy);
    assert_eq!(script.detach_calls(), 1);
    view.update(app, |view, _| {
        assert!(
            view.status
                .as_deref()
                .is_some_and(|status| status.contains("fixture tmux detach failed"))
        );
    });
}

/// 离开原目标后到达的 kill 失败按完整目标暂存：B 不显示 A 的失败，
/// 返回 A 时只显示一次，再次往返不重放。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_deferred_kill_feedback_is_scoped_to_original_target_once(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let host_a = HostId::new();
    let host_b = HostId::new();
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_host_list_reply(host_a.clone(), ports::TmuxListReply::Success)
        .with_host_list_reply(host_b.clone(), ports::TmuxListReply::Success)
        .with_kill_reply(ports::TmuxKillReply::Error);
    let (runtime, view) = panel_with_commands(app, script.clone());
    let tab_id = TabId::new();
    let attach_a = binding(&tab_id, Some(&host_a), None, PaneId::new());
    let attach_b = binding(&tab_id, Some(&host_b), None, PaneId::new());

    view.update(app, |view, cx| {
        view.active = true;
        view.initialized = true;
        view.tool_rail_target = Some(ToolRailOperationTarget::Ssh {
            host_id: host_a.clone(),
        });
        view.tmux_attach_binding = Some(attach_a.clone());
        let session = tmux_session_with_identity("$1", "work", "ssh-a", 100);
        let ready = view.tmux.begin(view.target_generation);
        assert!(view.tmux.complete(
            &ready,
            Ok(TmuxSnapshot {
                capability: TmuxCapability {
                    target_ref: "ssh-a".to_owned(),
                    available: true,
                    version: Some("tmux 3.4".to_owned()),
                    reason: None,
                },
                sessions: vec![session.clone()],
            }),
        ));
        view.tmux_dialog_context = Some((view.tmux_target(), view.tmux_view_generation));
        view.tmux_dialog_session = Some(session);
        view.tmux_pending_kill = Some("$1".to_owned());
        view.kill_tmux_session("$1".to_owned(), cx);
        // 写回前切到 B：A 的失败不能串到 B。
        view.set_tool_rail_target(
            ToolRailOperationTarget::Ssh {
                host_id: host_b.clone(),
            },
            cx,
        );
        view.set_tmux_attach_binding(Some(attach_b.clone()), cx);
    });
    pump_until(app, &runtime, &view, |view| !view.busy);
    assert_eq!(script.kill_calls(), 1);
    view.update(app, |view, _| {
        assert_eq!(view.tmux_target().host_id.as_ref(), Some(&host_b));
        assert!(view.status.is_none());
        assert_eq!(view.deferred_tmux_feedback.len(), 1);
    });

    // 返回 A：显示一次后即移除，不自动重放写操作。
    view.update(app, |view, cx| {
        view.set_tool_rail_target(
            ToolRailOperationTarget::Ssh {
                host_id: host_a.clone(),
            },
            cx,
        );
        view.set_tmux_attach_binding(Some(attach_a), cx);
    });
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Ready
    });
    view.update(app, |view, _| {
        assert_eq!(view.tmux_target().host_id.as_ref(), Some(&host_a));
        assert!(
            view.status
                .as_deref()
                .is_some_and(|status| status.contains("fixture tmux kill failed"))
        );
        assert!(view.deferred_tmux_feedback.is_empty());
    });

    // 再次往返 A→B→A 不重放历史提示，也不重新执行 kill。
    view.update(app, |view, cx| {
        view.set_tool_rail_target(
            ToolRailOperationTarget::Ssh {
                host_id: host_b.clone(),
            },
            cx,
        );
    });
    view.update(app, |view, cx| {
        view.set_tool_rail_target(
            ToolRailOperationTarget::Ssh {
                host_id: host_a.clone(),
            },
            cx,
        );
    });
    pump_until(app, &runtime, &view, |view| {
        view.tmux.phase() == SectionPhase::Ready
    });
    assert_eq!(script.kill_calls(), 1);
    view.update(app, |view, _| {
        assert!(view.status.is_none());
        assert!(view.deferred_tmux_feedback.is_empty());
    });
}

/// 面板隐藏期间到达的写回执不能因隐藏期状态清理而丢失：重显后显示一次。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_hidden_detach_feedback_survives_until_visible(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_list_replies([ports::TmuxListReply::Success])
        .with_detach_reply(ports::TmuxKillReply::Error);
    let (runtime, view, _tab, _attach) = prepared_local_panel(app, script.clone(), None);

    view.update(app, |view, cx| {
        view.detach_tmux_session("$1".to_owned(), cx);
        view.set_active(false, cx);
    });
    pump_until(app, &runtime, &view, |view| !view.busy);
    assert_eq!(script.detach_calls(), 1);
    view.update(app, |view, _| {
        assert_eq!(view.deferred_tmux_feedback.len(), 1);
    });

    view.update(app, |view, cx| view.set_active(true, cx));
    view.update(app, |view, _| {
        assert!(
            view.status
                .as_deref()
                .is_some_and(|status| status.contains("fixture tmux detach failed"))
        );
        assert!(view.deferred_tmux_feedback.is_empty());
    });
}

/// 紧凑布局成功回执没有文案，但必须保留旧语义清掉残留的取消等待提示，
/// 不能让“已请求取消”在远端实际成功后永久残留；同目标换 pane 不把它恢复或转移。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_compact_success_clears_cancel_wait_status(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_list_replies([ports::TmuxListReply::Success]);
    let (runtime, view, tab_id, _attach) = prepared_local_panel(app, script.clone(), None);

    view.update(app, |view, cx| {
        view.set_compact_layout(true, cx);
        view.detach_tmux_session("$1".to_owned(), cx);
    });
    // 先让 GPUI 任务把真实 detach 提交给执行器，再由 Tokio 驱动命令完成。若在同一个
    // 同步块里立刻请求取消，协作取消会在命令派发前生效，远端根本不会执行，也就没有
    // “成功回执”可用来验证取消等待提示的清理。
    app.run_until_parked();
    runtime.block_on(tokio::task::yield_now());
    assert_eq!(script.detach_calls(), 1);
    // 成功回执已经产生但尚未写回视图时请求取消：模拟“远端成功 + 正在等待回执”的真实
    // 竞态，compact 成功必须清除该残留提示。
    view.update(app, |view, cx| {
        view.cancel_operation_mutation(cx);
        assert!(
            view.status
                .as_deref()
                .is_some_and(|status| status.contains("已请求取消")
                    || status.contains("Cancellation requested")),
            "cancel wait status must be present before the receipt arrives"
        );
    });
    pump_until(app, &runtime, &view, |view| !view.busy);
    assert_eq!(script.detach_calls(), 1);
    view.update(app, |view, _| {
        // 远端实际成功：取消等待文案必须消失，不能与真实终态矛盾。
        assert!(view.status.is_none());
    });

    // 同目标换 pane 后没有暂存的成功文案被误投：status 保持为空。
    view.update(app, |view, cx| {
        view.set_tmux_attach_binding(Some(binding(&tab_id, None, None, PaneId::new())), cx);
        assert!(view.status.is_none());
        assert!(view.deferred_tmux_feedback.is_empty());
    });
}

/// 同身份覆盖或容量溢出丢弃的未展示结果必须显式可见：重显时拼接一条丢失提示，
/// 只报条数，不携带其它目标的结果内容，也不重放任何写操作。
///
/// @author kongweiguang
#[gpui_kit::test]
fn async_overflow_loss_is_reported_explicitly_once(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let script = ports::TmuxCommandScript::new(ports::TmuxProbeReply::Available)
        .with_list_replies([ports::TmuxListReply::Success]);
    let (runtime, view, tab_id, _attach) = prepared_local_panel(app, script.clone(), None);

    // 隐藏面板期间的真实 detach 回执先进入暂存；随后同身份回执覆盖它，再叠加跨目标
    // 条目直到容量溢出。三种丢弃都必须计入丢失，且最新一条仍可显示。
    view.update(app, |view, cx| {
        view.detach_tmux_session("$1".to_owned(), cx);
        view.set_active(false, cx);
    });
    pump_until(app, &runtime, &view, |view| !view.busy);
    assert_eq!(script.detach_calls(), 1);
    view.update(app, |view, _| {
        let frozen_target = view.tmux_target();
        // 基线必须是真实异步 detach 回执，而不是测试自己伪造的条目。
        assert_eq!(view.deferred_tmux_feedback.len(), 1);
        let baseline = view
            .deferred_tmux_feedback
            .front()
            .expect("real detach receipt is queued");
        assert_eq!(baseline.target, frozen_target);
        assert_eq!(
            baseline.identity,
            TmuxMutationIdentity::Detach {
                session_id: "$1".to_owned(),
            }
        );
        assert_eq!(view.deferred_tmux_feedback_losses, 0);

        // 同目标同身份 8 条：每条覆盖前一条，包括真实 detach 回执，共丢弃 8 条；
        // 队列只保留最新一条。断言覆盖“真实回执也计入丢失”，不能少算成 7。
        for index in 0..8 {
            view.apply_tmux_mutation_feedback(
                &frozen_target,
                TmuxMutationIdentity::Detach {
                    session_id: "$1".to_owned(),
                },
                format!("hidden {index}"),
            );
            assert!(
                view.deferred_tmux_feedback.len() <= 8,
                "deferred feedback stays bounded"
            );
        }
        assert_eq!(view.deferred_tmux_feedback.len(), 1);
        assert_eq!(view.deferred_tmux_feedback_losses, 8);
        let latest = view
            .deferred_tmux_feedback
            .front()
            .expect("latest same-identity receipt is kept");
        assert_eq!(latest.message, "hidden 7");
        assert_eq!(latest.target, frozen_target);

        // 跨目标 8 条 Kill 压满容量：最新一条仍被保留，队首 hidden 7 因 FIFO 淘汰计入丢失。
        let other_target = kerminal_domain::operations::TmuxTarget {
            host_id: Some(kerminal_domain::identity::HostId::new()),
            ..kerminal_domain::operations::TmuxTarget::default()
        };
        for index in 0..8 {
            view.apply_tmux_mutation_feedback(
                &other_target,
                TmuxMutationIdentity::Kill {
                    session_id: format!("${index}"),
                },
                format!("overflow {index}"),
            );
            assert!(
                view.deferred_tmux_feedback.len() <= 8,
                "cross-target feedback cannot grow the backlog past capacity"
            );
        }
        assert_eq!(view.deferred_tmux_feedback.len(), 8);
        assert_eq!(view.deferred_tmux_feedback_losses, 9);
        assert!(
            view.deferred_tmux_feedback
                .iter()
                .all(|entry| entry.target == other_target),
            "the local entry is the FIFO victim, never a silently kept duplicate"
        );

        // 再投一条本地同身份回执：队列已满，最早一条跨目标结果被淘汰，最新本地回执可见。
        view.apply_tmux_mutation_feedback(
            &frozen_target,
            TmuxMutationIdentity::Detach {
                session_id: "$1".to_owned(),
            },
            "hidden 8".to_owned(),
        );
        assert_eq!(view.deferred_tmux_feedback.len(), 8);
        assert_eq!(view.deferred_tmux_feedback_losses, 10);
        let newest = view
            .deferred_tmux_feedback
            .back()
            .expect("newest local receipt is queued");
        assert_eq!(newest.message, "hidden 8");
        assert_eq!(newest.target, frozen_target);
    });

    // 重显时隐藏期最新回执与丢失提示一次可见；其它目标的结果内容不串投给当前目标。
    view.update(app, |view, cx| view.set_active(true, cx));
    view.update(app, |view, _| {
        let status = view.status.clone().expect("visible loss notice");
        assert!(status.contains("hidden 8"), "latest receipt is shown");
        assert!(
            !status.contains("hidden 7"),
            "the replaced and evicted receipts are not replayed: {status}"
        );
        assert!(
            status.contains("未展示") || status.contains("were not shown"),
            "loss notice must be explicit: {status}"
        );
        assert!(
            status.contains("10"),
            "loss notice must report the exact dropped count: {status}"
        );
        assert_eq!(view.deferred_tmux_feedback_losses, 0);
        assert_eq!(
            view.deferred_tmux_feedback.len(),
            7,
            "only the other-target entries stay queued"
        );
        assert!(
            view.deferred_tmux_feedback
                .iter()
                .all(|entry| entry.message != "hidden 8"),
            "consumed local receipt is removed"
        );
        // 其它目标的结果内容不伪投给当前目标。
        assert!(!status.contains("overflow"));
    });

    // 再次重显（激活往返）不重放丢失提示，也不重放写操作。
    view.update(app, |view, cx| {
        view.set_active(false, cx);
        view.set_active(true, cx);
    });
    view.update(app, |view, _| {
        // 丢失提示只报一次：若重放，同一 status 里会出现第二份提示文案。
        let status = view.status.as_deref().unwrap_or_default();
        let notice_markers = ["未展示", "were not shown"];
        let notices = notice_markers
            .iter()
            .map(|marker| status.matches(*marker).count())
            .max()
            .unwrap_or(0);
        assert_eq!(notices, 1, "loss notice is not replayed on re-show");
        assert_eq!(script.detach_calls(), 1, "no write is replayed");
        assert_eq!(view.deferred_tmux_feedback_losses, 0);
        assert_eq!(view.deferred_tmux_feedback.len(), 7);
    });
    // 同目标换 pane 只清当前 status，不恢复已消费的本地回执或丢失计数。
    view.update(app, |view, cx| {
        view.set_tmux_attach_binding(Some(binding(&tab_id, None, None, PaneId::new())), cx);
    });
    view.update(app, |view, _| {
        assert!(view.status.is_none());
        assert_eq!(view.deferred_tmux_feedback_losses, 0);
        assert_eq!(view.deferred_tmux_feedback.len(), 7);
        assert_eq!(script.detach_calls(), 1);
    });
}
