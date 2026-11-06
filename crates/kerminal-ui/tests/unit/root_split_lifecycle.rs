//! 同 SessionId 重连后目录与输出订阅的 Root 回归；所有服务和终端引擎均隔离在内存。
//!
//! @author kongweiguang

use super::*;

use std::{collections::BTreeMap, ffi::OsString};

use kerminal_application::sessions::{
    SessionChange, SessionChangeKind, TerminalCommandAvailability, TerminalCommandUnavailableReason,
};
use kerminal_domain::{connections::RemoteShellDialect, sessions::SessionLifecycle};
use kerminal_terminal::{TerminalEngine, TerminalLimits};

use crate::root::{
    WorkspaceTerminalOpenKind, terminal_open::WorkspaceTerminalOpenRequest,
    workspace_pane_for_terminal_target, workspace_runtime::WorkspaceRuntime,
};
use crate::{
    features::workspace_panes::WorkspacePaneEvent, root::workspace_render::WorkspaceRenderEvent,
};

/// 测试仅显式启用单个可订阅会话；认证失败样本使用 None 保持原有封闭服务边界。
pub(super) struct LiveSession {
    pub(super) info: SessionInfo,
    pub(super) generation: u64,
    pub(super) engine: TerminalEngine,
    pub(super) changes: tokio::sync::watch::Sender<SessionChange>,
    pub(super) restart_wait_for_cancellation: Option<CancellationToken>,
}

impl LiveSession {
    /// 保持 session 身份不变替换内存引擎，模拟真实 transport 重连和较小的新 revision。
    ///
    /// @author kongweiguang
    fn new(target: SessionTarget) -> Self {
        let session_id = SessionId::new();
        let size = TerminalSize::default();
        let (changes, _) = tokio::sync::watch::channel(SessionChange {
            revision: 0,
            kind: SessionChangeKind::Lifecycle,
        });
        Self {
            engine: TerminalEngine::new(session_id.clone(), size, TerminalLimits::default())
                .unwrap(),
            info: SessionInfo {
                session_id,
                target,
                owner: SessionOwner::Ui {
                    tab_id: TabId::new(),
                    pane_id: PaneId::new(),
                },
                size,
                lifecycle: SessionLifecycle::Running,
                exit_status: None,
            },
            generation: 1,
            changes,
            restart_wait_for_cancellation: None,
        }
    }

    /// 只提供可信 owner/generation；不伪造 shell 提示符或开启建议请求。
    ///
    /// @author kongweiguang
    pub(super) fn command_context(&self) -> TerminalCommandContext {
        TerminalCommandContext {
            session_id: self.info.session_id.clone(),
            owner: self.info.owner.clone(),
            session_generation: self.generation,
            prompt_epoch: 0,
            input_generation: 0,
            shell_dialect: RemoteShellDialect::Posix,
            prompt_ready: false,
            prompt_line_empty: false,
            alternate_screen: false,
            availability: TerminalCommandAvailability::Unavailable(
                TerminalCommandUnavailableReason::ShellIntegrationNotObserved,
            ),
        }
    }
}

/// 所有恢复回归共用生产打开入口，避免手动装配 Ready 状态绕过 watcher 与首帧身份核验。
///
/// @author kongweiguang
fn open_live_terminal(
    root: &Entity<RootView>,
    visual: &mut VisualTestContext,
    target: SessionTarget,
) -> PaneId {
    visual.update(|_, app| {
        root.update(app, |root, cx| {
            let (pane, _) = workspace_pane_for_terminal_target(Some(&target), "fixture", None);
            let pane_id = pane.id.clone();
            let tab_id = TabId::new();
            let mut controller = WorkspaceController::from_missing().unwrap();
            let mut workspace = controller.snapshot().clone();
            workspace.active_tab_id = Some(tab_id.clone());
            workspace.focused_pane_id = Some(pane_id.clone());
            workspace.terminal_tabs = vec![WorkspaceSessionTab::Terminal {
                id: tab_id.clone(),
                tab_group_id: None,
                title: "fixture".into(),
                machine_id: pane.machine_id.clone(),
                layout: kerminal_domain::workspace::TerminalLayoutNode::pane(pane_id.clone()),
            }];
            workspace.terminal_panes = vec![pane];
            controller.synchronize_snapshot(workspace).unwrap();
            root.workspace_runtime =
                WorkspaceRuntime::from_snapshot(controller.snapshot()).unwrap();
            root.workspace_controller = Some(controller);
            root.workspace_load_pending = false;
            root.center_panel = CenterPanel::Terminal;
            let (generation, cancellation) = root.begin_workspace_restore(&pane_id);
            root.open_workspace_terminal_pane(
                WorkspaceTerminalOpenRequest {
                    tab_id,
                    pane_id: pane_id.clone(),
                    kind: WorkspaceTerminalOpenKind::Direct(target),
                    size: TerminalSize::default(),
                    lease: None,
                    transient: false,
                    generation,
                    cancellation,
                },
                cx,
            )
            .unwrap();
            pane_id
        })
    })
}

/// 真实生产打开路径安装 watcher；Lifecycle 后继续 Output 才能刷新目录，避免只测条件表达式。
///
/// @author kongweiguang
#[gpui_kit::test]
fn lifecycle_keeps_subscription_and_later_output_updates_directory(app: &mut TestAppContext) {
    let (runtime, root, sessions, _, visual) = fixture(app);
    settle(&runtime, visual);
    let target = SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("fixture-shell"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    });
    let mut live = LiveSession::new(target.clone());
    live.engine
        .process_output(b"\x1b]7;file:///old-process\x07");
    let session_id = live.info.session_id.clone();
    *sessions.live.lock().unwrap() = Some(live);
    let pane_id = open_live_terminal(&root, visual, target);
    settle(&runtime, visual);
    visual.update(|_, app| {
        root.update(app, |root, _| {
            assert!(root.workspace_runtime.pane_directory_is_current(&pane_id));
            assert_eq!(
                root.workspace_runtime
                    .pane(&pane_id)
                    .unwrap()
                    .current_working_directory
                    .as_deref(),
                Some("/old-process")
            );
        })
    });
    {
        let mut fixture = sessions.live.lock().unwrap();
        let live = fixture.as_mut().unwrap();
        live.generation += 1;
        live.engine = TerminalEngine::new(
            session_id.clone(),
            live.info.size,
            TerminalLimits::default(),
        )
        .unwrap();
        live.changes
            .send(SessionChange {
                revision: 1,
                kind: SessionChangeKind::Lifecycle,
            })
            .unwrap();
    }
    settle(&runtime, visual);
    visual.update(|_, app| {
        root.update(app, |root, _| {
            assert!(root.workspace_runtime.pane_directory_is_current(&pane_id));
            assert!(
                root.workspace_runtime
                    .pane(&pane_id)
                    .unwrap()
                    .current_working_directory
                    .is_none()
            );
            assert_eq!(
                root.terminal_view_bindings
                    .get(&session_id)
                    .unwrap()
                    .session_generation,
                2
            );
        })
    });
    {
        let mut fixture = sessions.live.lock().unwrap();
        let live = fixture.as_mut().unwrap();
        live.engine
            .process_output(b"\x1b]7;file:///new-process\x07");
        live.changes
            .send(SessionChange {
                revision: 2,
                kind: SessionChangeKind::Output,
            })
            .unwrap();
    }
    settle(&runtime, visual);
    visual.update(|_, app| {
        root.update(app, |root, _| {
            assert_eq!(
                root.workspace_runtime
                    .pane(&pane_id)
                    .unwrap()
                    .current_working_directory
                    .as_deref(),
                Some("/new-process")
            );
            root.session_watch_cancellations
                .get(&session_id)
                .unwrap()
                .cancel();
        })
    });
    settle(&runtime, visual);
}

/// 重连失败和取消走真实 Root 结算与补读，验证门禁恢复且不会偷偷创建替代 session。
/// 取消必须等 mock 已进入端口后再请求，确保覆盖在途取消而非执行前取消的巧合。
///
/// @author kongweiguang
#[gpui_kit::test]
fn failed_and_cancelled_reconnect_rebind_original_session_directory(app: &mut TestAppContext) {
    let (runtime, root, sessions, _, visual) = fixture(app);
    settle(&runtime, visual);
    let target = SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("fixture-shell"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    });
    let mut live = LiveSession::new(target.clone());
    live.engine
        .process_output(b"\x1b]7;file:///stable-directory\x07");
    let session_id = live.info.session_id.clone();
    *sessions.live.lock().unwrap() = Some(live);
    let pane_id = open_live_terminal(&root, visual, target);
    settle(&runtime, visual);

    for cancel in [false, true] {
        let entered = cancel.then(CancellationToken::new);
        sessions
            .live
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .restart_wait_for_cancellation = entered.clone();
        let (binding, attempt_id) = visual.update(|window, app| {
            root.update(app, |root, cx| {
                assert!(root.workspace_pane_can_split(&pane_id));
                let pane = root.workspace_runtime.pane(&pane_id).unwrap();
                let tab_id = pane.tab_id.clone();
                let binding = root.session_action_projection(&pane_id).unwrap().binding;
                root.reconnect_workspace_pane(
                    tab_id.clone(),
                    pane_id.clone(),
                    binding.clone(),
                    window,
                    cx,
                );
                let action = root.session_action_projection(&pane_id).unwrap();
                assert!(action.busy);
                assert!(!root.workspace_pane_can_split(&pane_id));
                assert!(!root.workspace_runtime.pane_directory_is_current(&pane_id));
                root.handle_workspace_render_event(
                    WorkspaceRenderEvent::Pane {
                        tab_id,
                        event: WorkspacePaneEvent::Split {
                            pane_id: pane_id.clone(),
                            direction: kerminal_domain::workspace::SplitDirection::Horizontal,
                            before: false,
                        },
                    },
                    window,
                    cx,
                );
                assert!(
                    root.workspace_controller
                        .as_ref()
                        .unwrap()
                        .pending_resource_operation()
                        .is_none()
                );
                (binding, action.attempt_id.unwrap())
            })
        });
        settle(&runtime, visual);
        if let Some(entered) = entered {
            assert!(entered.is_cancelled(), "必须实际进入重连端口后才触发取消");
            visual.update(|_, app| {
                root.update(app, |root, cx| {
                    assert!(root.session_action_projection(&pane_id).unwrap().busy);
                    root.cancel_session_reconnect(pane_id.clone(), binding, attempt_id, cx);
                })
            });
            settle(&runtime, visual);
        }
        visual.update(|_, app| {
            root.update(app, |root, _| {
                let action = root.session_action_projection(&pane_id).unwrap();
                assert!(!action.busy);
                assert!(root.workspace_pane_can_split(&pane_id));
                assert!(root.workspace_runtime.pane_directory_is_current(&pane_id));
                let pane = root.workspace_runtime.pane(&pane_id).unwrap();
                assert_eq!(pane.session_id.as_ref(), Some(&session_id));
                assert_eq!(
                    pane.current_working_directory.as_deref(),
                    Some("/stable-directory")
                );
                assert_eq!(
                    root.terminal_view_bindings
                        .get(&session_id)
                        .unwrap()
                        .session_generation,
                    1
                );
            })
        });
        assert_eq!(sessions.creates.lock().unwrap().len(), 1);
    }
    visual.update(|_, app| {
        root.update(app, |root, _| {
            root.session_watch_cancellations
                .get(&session_id)
                .unwrap()
                .cancel();
        })
    });
    settle(&runtime, visual);
}
