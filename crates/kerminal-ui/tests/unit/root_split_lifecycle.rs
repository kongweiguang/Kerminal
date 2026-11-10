//! 同 SessionId 重连后目录与输出订阅的 Root 回归；所有服务和终端引擎均隔离在内存。
//!
//! @author kongweiguang

use super::*;

// 复用同一内存终端及 Root，检查布局变化期间的右栏绑定与文件读取次数。
#[path = "root_sidebar_context.rs"]
mod sidebar_context;

use std::{collections::BTreeMap, ffi::OsString};

use kerminal_application::sessions::{
    SessionChange, SessionChangeKind, TerminalCommandAvailability, TerminalCommandUnavailableReason,
};
use kerminal_domain::{
    connections::RemoteShellDialect, sessions::SessionLifecycle, workspace::SplitDirection,
};
use kerminal_terminal::{TerminalEngine, TerminalLimits};

use crate::root::{
    WorkspaceTerminalOpenKind, terminal_open::WorkspaceTerminalOpenRequest,
    workspace_pane_for_terminal_target, workspace_runtime::WorkspaceRuntime,
};
use crate::{
    features::{
        workspace_controller::{WorkspaceResourceOperation, WorkspaceRestoreFailure},
        workspace_panes::WorkspacePaneEvent,
        workspace_tabs::WorkspaceTabEvent,
    },
    root::{
        workspace_close::{WorkspaceDeferredCloseIntent, WorkspaceResourceOperationEntry},
        workspace_render::WorkspaceRenderEvent,
    },
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

/// 用真实打开路径安装一个 Ready 本地 pane，返回稳定 tab/pane/session 身份；
/// 分屏与预检回归共用这一装配，避免各自伪造运行态。
fn open_ready_local_pane(
    root: &Entity<RootView>,
    visual: &mut VisualTestContext,
    runtime: &Runtime,
    sessions: &Arc<CapturingSessions>,
) -> (TabId, PaneId, SessionId) {
    settle(runtime, visual);
    let target = SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("fixture-shell"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    });
    *sessions.live.lock().unwrap() = Some(LiveSession::new(target.clone()));
    let source_pane = open_live_terminal(root, visual, target);
    settle(runtime, visual);
    visual.update(|_, app| {
        root.update(app, |root, _| {
            let pane = root
                .workspace_runtime
                .pane(&source_pane)
                .expect("source pane is ready");
            (
                pane.tab_id.clone(),
                source_pane.clone(),
                pane.session_id
                    .clone()
                    .expect("source pane owns a live session"),
            )
        })
    })
}

/// 用真实打开路径安装一个 Ready 本地 pane，再让 Root 产生未准入的分屏候选；
/// 后续失败/关闭测试只操作这个精确候选，不依赖网络或用户配置。
fn ready_split_candidate(
    root: &Entity<RootView>,
    visual: &mut VisualTestContext,
    runtime: &Runtime,
    sessions: &Arc<CapturingSessions>,
) -> (TabId, PaneId, WorkspaceResourceOperation) {
    let (tab_id, source_pane, _) = open_ready_local_pane(root, visual, runtime, sessions);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.handle_workspace_render_event(
                WorkspaceRenderEvent::Pane {
                    tab_id: tab_id.clone(),
                    event: WorkspacePaneEvent::Split {
                        pane_id: source_pane.clone(),
                        direction: SplitDirection::Vertical,
                        before: false,
                    },
                },
                window,
                cx,
            );
            let operation = root
                .workspace_controller
                .as_ref()
                .and_then(|controller| controller.pending_resource_operation())
                .cloned()
                .expect("split must produce a CreatePane candidate");
            (tab_id, source_pane, operation)
        })
    })
}

/// 已结束失败的空白候选不会再有清理回执；关闭它必须立即收尾并释放资源槽，
/// 而不是让 deferred close 永久等待。原 pane 与其它资源保持不变。
///
/// @author kongweiguang
#[gpui_kit::test]
fn completed_split_failure_closes_without_waiting_for_receipt(app: &mut TestAppContext) {
    let (runtime, root, sessions, _, visual) = fixture(app);
    let (tab_id, source_pane, operation) =
        ready_split_candidate(&root, visual, &runtime, &sessions);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let WorkspaceResourceOperation::CreatePane {
                operation_id, pane, ..
            } = &operation
            else {
                panic!("split must create a pane");
            };
            let candidate = pane.id.clone();
            root.workspace_controller
                .as_mut()
                .expect("workspace controller")
                .complete_resource_operation(
                    operation_id,
                    Err(WorkspaceRestoreFailure::ResourceFailure),
                )
                .unwrap();
            root.workspace_runtime
                .mark_restore_failed(&candidate)
                .unwrap();
            root.workspace_resource_operations
                .get_mut(&candidate)
                .expect("split resource entry")
                .try_admit_split_measurement(operation_id, 1);
            assert!(root.defer_workspace_split_close_intent(
                WorkspaceDeferredCloseIntent::Pane {
                    tab_id: tab_id.clone(),
                    pane_id: candidate.clone(),
                },
                true,
                None,
                None,
                window,
                cx,
            ));
            assert!(
                root.workspace_controller
                    .as_ref()
                    .unwrap()
                    .pending_resource_operation()
                    .is_none()
            );
            assert!(root.workspace_runtime.pane(&candidate).is_none());
            assert!(!root.workspace_resource_operations.contains_key(&candidate));
            assert!(root.workspace_runtime.pane(&source_pane).is_some());
        });
    });
}

/// 仍在途的候选即使已经测量，也必须等待真实清理回执；不能仅凭关闭意图移除资源或候选。
///
/// @author kongweiguang
#[gpui_kit::test]
fn in_flight_split_close_still_waits_for_receipt(app: &mut TestAppContext) {
    let (runtime, root, sessions, _, visual) = fixture(app);
    let (tab_id, _, operation) = ready_split_candidate(&root, visual, &runtime, &sessions);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let WorkspaceResourceOperation::CreatePane {
                operation_id, pane, ..
            } = &operation
            else {
                panic!("split must create a pane");
            };
            let candidate = pane.id.clone();
            root.workspace_resource_operations
                .get_mut(&candidate)
                .expect("split resource entry")
                .try_admit_split_measurement(operation_id, 1);
            assert!(root.defer_workspace_split_close_intent(
                WorkspaceDeferredCloseIntent::Pane {
                    tab_id: tab_id.clone(),
                    pane_id: candidate.clone(),
                },
                true,
                None,
                None,
                window,
                cx,
            ));
            assert!(
                root.workspace_controller
                    .as_ref()
                    .unwrap()
                    .pending_resource_operation()
                    .is_some()
            );
            assert!(root.workspace_runtime.pane(&candidate).is_some());
        });
    });
}

/// Ready 收尾失败必须写回 failed CreatePane：候选仍可重试、标签切换不被锁死。
/// @author kongweiguang
#[gpui_kit::test]
fn ready_writeback_failure_stays_retryable_and_activation_is_allowed(app: &mut TestAppContext) {
    let (runtime, root, sessions, _, visual) = fixture(app);
    let (tab_id, _, operation) = ready_split_candidate(&root, visual, &runtime, &sessions);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let WorkspaceResourceOperation::CreatePane { pane, .. } = &operation else {
                panic!("split must create a pane");
            };
            let candidate = pane.id.clone();
            root.workspace_runtime
                .mark_restore_failed(&candidate)
                .unwrap();
            root.fail_workspace_split_after_ready(
                &tab_id,
                &candidate,
                crate::locale::text("fixture ready failure", "fixture ready failure"),
                cx,
            );
            assert!(
                matches!(
                    root.workspace_controller
                        .as_ref()
                        .unwrap()
                        .retry_pending_resource_operation(),
                    Some(WorkspaceResourceOperation::CreatePane { .. })
                ),
                "Ready 收尾失败必须留下可重试失败项"
            );
            assert!(
                root.handle_workspace_tab_event(
                    &WorkspaceTabEvent::Activate(tab_id.clone()),
                    window,
                    cx,
                ),
                "失败候选不能阻止标签激活"
            );
            assert_eq!(
                root.workspace_controller
                    .as_ref()
                    .unwrap()
                    .snapshot()
                    .active_tab_id
                    .as_ref(),
                Some(&tab_id)
            );
        });
    });
}

/// 带 retained session 的失败候选关闭时必须走 CloseSession，而不是等待创建回执。
/// @author kongweiguang
#[gpui_kit::test]
fn closing_failed_split_with_session_launches_close_session(app: &mut TestAppContext) {
    let (runtime, root, sessions, _, visual) = fixture(app);
    let (tab_id, _, operation) = ready_split_candidate(&root, visual, &runtime, &sessions);
    let session_id = SessionId::new();
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let WorkspaceResourceOperation::CreatePane {
                operation_id, pane, ..
            } = &operation
            else {
                panic!("split must create a pane");
            };
            let candidate = pane.id.clone();
            let target = SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("fixture-cleanup"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            });
            root.workspace_runtime
                .retain_cleanup_session(
                    &candidate,
                    session_id.clone(),
                    target,
                    TerminalSize::default(),
                    None,
                    "fixture retained session".into(),
                )
                .unwrap();
            root.workspace_controller
                .as_mut()
                .unwrap()
                .fail_pending_create_pane(
                    &tab_id,
                    &candidate,
                    WorkspaceRestoreFailure::ResourceFailure,
                );
            root.workspace_resource_operations
                .get_mut(&candidate)
                .expect("split resource entry")
                .try_admit_split_measurement(operation_id, 1);
            assert!(root.defer_workspace_split_close_intent(
                WorkspaceDeferredCloseIntent::Pane {
                    tab_id: tab_id.clone(),
                    pane_id: candidate.clone(),
                },
                true,
                None,
                None,
                window,
                cx,
            ));
            assert!(
                matches!(
                    root.workspace_controller
                        .as_ref()
                        .unwrap()
                        .pending_resource_operation(),
                    Some(WorkspaceResourceOperation::ClosePane { pane_id, .. })
                        if *pane_id == candidate
                ),
                "失败候选必须替换为 ClosePane，而不是继续等待 CreatePane ACK"
            );
        });
    });
    settle(&runtime, visual);
    assert!(
        sessions.closed.lock().unwrap().contains(&session_id),
        "带 session 的失败候选必须实际启动关闭"
    );
}

/// failed ClosePane 放弃分屏后仍占用真实 session；标签选择必须放行，pane/tab 关闭
/// 必须回到同一 ClosePane 重试，成功后才移除 pending 与状态记录。
///
/// @author kongweiguang
#[gpui_kit::test]
fn failed_close_pane_keeps_selection_and_retries_same_operation(app: &mut TestAppContext) {
    use std::sync::atomic::Ordering;

    let (runtime, root, sessions, _, visual) = fixture(app);
    let (tab_id, source_pane, operation) =
        ready_split_candidate(&root, visual, &runtime, &sessions);
    let session_id = SessionId::new();
    let (candidate, close_operation_id) = visual.update(|window, app| {
        root.update(app, |root, cx| {
            let WorkspaceResourceOperation::CreatePane {
                operation_id, pane, ..
            } = &operation
            else {
                panic!("split must create a pane");
            };
            let candidate = pane.id.clone();
            let target = SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("fixture-close-failure"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            });
            root.workspace_runtime
                .retain_cleanup_session(
                    &candidate,
                    session_id.clone(),
                    target,
                    TerminalSize::default(),
                    None,
                    "fixture retained session".into(),
                )
                .unwrap();
            root.workspace_controller
                .as_mut()
                .unwrap()
                .fail_pending_create_pane(
                    &tab_id,
                    &candidate,
                    WorkspaceRestoreFailure::ResourceFailure,
                );
            root.workspace_resource_operations
                .get_mut(&candidate)
                .expect("split resource entry")
                .try_admit_split_measurement(operation_id, 1);
            // 无关资源记录必须跨重试/成功收尾保持不变。
            root.workspace_resource_operations.insert(
                source_pane.clone(),
                WorkspaceResourceOperationEntry::new("unrelated-operation".into()),
            );
            assert!(root.defer_workspace_split_close_intent(
                WorkspaceDeferredCloseIntent::Pane {
                    tab_id: tab_id.clone(),
                    pane_id: candidate.clone(),
                },
                true,
                None,
                None,
                window,
                cx,
            ));
            let pending = root
                .workspace_controller
                .as_ref()
                .unwrap()
                .pending_resource_operation()
                .cloned()
                .expect("replacing failed split must keep a close operation");
            let WorkspaceResourceOperation::ClosePane {
                operation_id: close_id,
                pane_id,
                ..
            } = pending
            else {
                panic!("failed split must be replaced with ClosePane");
            };
            assert_eq!(pane_id, candidate);
            (candidate, close_id)
        })
    });
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            assert!(
                matches!(
                    root.workspace_controller
                        .as_ref()
                        .unwrap()
                        .retry_pending_resource_operation(),
                    Some(WorkspaceResourceOperation::ClosePane {
                        operation_id,
                        pane_id,
                        ..
                    }) if operation_id.as_str() == close_operation_id.as_str() && pane_id == candidate
                ),
                "close failure must leave the same ClosePane retryable"
            );
            let pane = root.workspace_runtime.pane(&candidate).unwrap();
            assert_eq!(
                pane.phase,
                crate::root::workspace_runtime::RuntimePanePhase::CloseFailed
            );
            assert!(
                pane.status
                    .as_ref()
                    .is_some_and(|status| {
                    let status = status.as_ref();
                    status.contains("再次关闭该标签重试") || status.contains("close this tab again to retry")
                }),
                "close failure must point at the retry entry, status={:?}",
                pane.status
            );
            assert!(
                root.handle_workspace_tab_event(
                    &WorkspaceTabEvent::Activate(tab_id.clone()),
                    window,
                    cx,
                ),
                "failed ClosePane must not lock tab activation"
            );
            // 重复关闭不能把 failed ClosePane 再替换成另一项操作。
            assert!(!root.defer_workspace_split_close_intent(
                WorkspaceDeferredCloseIntent::Pane {
                    tab_id: tab_id.clone(),
                    pane_id: candidate.clone(),
                },
                true,
                None,
                None,
                window,
                cx,
            ));
            assert_eq!(
                root.workspace_controller
                    .as_ref()
                    .unwrap()
                    .pending_resource_operation()
                    .unwrap()
                    .operation_id(),
                close_operation_id
            );
        });
    });

    // 标签关闭应重新派发同一 ClosePane；替身仍失败，关闭调用次数必须增加。
    let closes_before_retry = sessions.closed.lock().unwrap().len();
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.handle_workspace_tab_event(&WorkspaceTabEvent::Close(tab_id.clone()), window, cx);
        })
    });
    settle(&runtime, visual);
    {
        let closed = sessions.closed.lock().unwrap();
        assert!(closed.len() > closes_before_retry);
        assert_eq!(closed.last(), Some(&session_id));
    }
    visual.update(|_, app| {
        root.update(app, |root, _| {
            assert!(
                matches!(
                    root.workspace_controller
                        .as_ref()
                        .unwrap()
                        .retry_pending_resource_operation(),
                    Some(WorkspaceResourceOperation::ClosePane { operation_id, .. })
                        if operation_id.as_str() == close_operation_id.as_str()
                ),
                "failed retry must keep the original ClosePane"
            );
        })
    });

    // CloseFailed 占位的“重试恢复”按钮也必须回到同一 ClosePane，而不是报“不是分屏创建”。
    let closes_before_placeholder_retry = sessions.closed.lock().unwrap().len();
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.handle_workspace_render_event(
                WorkspaceRenderEvent::RetryPane {
                    tab_id: tab_id.clone(),
                    pane_id: candidate.clone(),
                },
                window,
                cx,
            );
        })
    });
    settle(&runtime, visual);
    {
        let closed = sessions.closed.lock().unwrap();
        assert!(closed.len() > closes_before_placeholder_retry);
        assert_eq!(closed.last(), Some(&session_id));
    }

    // 替身改为成功后，从 pane 关闭入口重试同一操作，pending 与 entry 才收敛。
    sessions.close_succeeds.store(true, Ordering::Relaxed);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.handle_workspace_render_event(
                WorkspaceRenderEvent::Pane {
                    tab_id: tab_id.clone(),
                    event: WorkspacePaneEvent::Close(candidate.clone()),
                },
                window,
                cx,
            );
        })
    });
    settle(&runtime, visual);
    visual.update(|_, app| {
        root.update(app, |root, _| {
            assert!(
                root.workspace_controller
                    .as_ref()
                    .unwrap()
                    .pending_resource_operation()
                    .is_none(),
                "successful retry must clear the pending close"
            );
            assert!(!root.workspace_resource_operations.contains_key(&candidate));
            assert!(root.workspace_runtime.pane(&candidate).is_none());
            assert!(root.workspace_runtime.pane(&source_pane).is_some());
            assert!(
                root.workspace_resource_operations
                    .contains_key(&source_pane)
            );
        })
    });
}

/// 无 session/orphan 的失败候选在新建终端前被精确丢弃，不删除其它资源记录。
/// @author kongweiguang
#[gpui_kit::test]
fn reserve_new_terminal_discards_failed_empty_split(app: &mut TestAppContext) {
    let (runtime, root, sessions, _, visual) = fixture(app);
    let (tab_id, source_pane, operation) =
        ready_split_candidate(&root, visual, &runtime, &sessions);
    visual.update(|_, app| {
        root.update(app, |root, cx| {
            let WorkspaceResourceOperation::CreatePane { pane, .. } = &operation else {
                panic!("split must create a pane");
            };
            let candidate = pane.id.clone();
            root.workspace_runtime
                .mark_restore_failed(&candidate)
                .unwrap();
            root.workspace_controller
                .as_mut()
                .unwrap()
                .fail_pending_create_pane(
                    &tab_id,
                    &candidate,
                    WorkspaceRestoreFailure::ResourceFailure,
                );
            root.workspace_resource_operations.insert(
                source_pane.clone(),
                WorkspaceResourceOperationEntry::new("unrelated-operation".into()),
            );
            let (new_pane, _) =
                workspace_pane_for_terminal_target(None, "after-failed-split", None);
            assert!(
                root.reserve_new_workspace_terminal_tab(
                    new_pane,
                    "After failed split".into(),
                    TerminalSize::default(),
                    false,
                    cx,
                )
                .is_ok(),
                "无资源的失败候选不能阻塞新终端"
            );
            assert!(
                root.workspace_controller
                    .as_ref()
                    .unwrap()
                    .pending_resource_operation()
                    .is_none()
            );
            assert!(root.workspace_runtime.pane(&candidate).is_none());
            assert!(!root.workspace_resource_operations.contains_key(&candidate));
            assert_eq!(
                root.workspace_resource_operations
                    .get(&source_pane)
                    .expect("unrelated entry remains")
                    .operation_id(),
                "unrelated-operation"
            );
        });
    });
}

/// 失败候选的准入失败仍会精确清理，且不影响其它资源 owner。
///
/// @author kongweiguang
#[gpui_kit::test]
fn split_admission_failure_discards_exact_pending_and_empty_candidate(app: &mut TestAppContext) {
    let (runtime, root, sessions, _, visual) = fixture(app);
    let (_, _, operation) = ready_split_candidate(&root, visual, &runtime, &sessions);
    visual.update(|_, app| {
        root.update(app, |root, cx| {
            let WorkspaceResourceOperation::CreatePane {
                operation_id, pane, ..
            } = &operation
            else {
                panic!("split must create a pane");
            };
            let candidate = pane.id.clone();
            root.fail_workspace_split_admission(
                operation_id,
                &candidate,
                "fixture admission failure".into(),
                cx,
            );
            assert!(
                root.workspace_controller
                    .as_ref()
                    .unwrap()
                    .pending_resource_operation()
                    .is_none()
            );
            assert!(root.workspace_runtime.pane(&candidate).is_none());
            assert!(!root.workspace_resource_operations.contains_key(&candidate));
            let (new_pane, _) =
                workspace_pane_for_terminal_target(None, "after-admission-failure", None);
            assert!(
                root.reserve_new_workspace_terminal_tab(
                    new_pane,
                    "After failure".into(),
                    TerminalSize::default(),
                    false,
                    cx,
                )
                .is_ok()
            );
        });
    });
}

/// 失败候选仍被另一资源操作持有时，准入失败只能丢弃本次 pending，
/// 不能删除别人的 pane 或操作记录。
///
/// @author kongweiguang
#[gpui_kit::test]
fn split_admission_failure_preserves_other_resource_owner(app: &mut TestAppContext) {
    let (runtime, root, sessions, _, visual) = fixture(app);
    let (_, _, operation) = ready_split_candidate(&root, visual, &runtime, &sessions);
    visual.update(|_, app| {
        root.update(app, |root, cx| {
            let WorkspaceResourceOperation::CreatePane {
                operation_id, pane, ..
            } = &operation
            else {
                panic!("split must create a pane");
            };
            let candidate = pane.id.clone();
            root.workspace_resource_operations.insert(
                candidate.clone(),
                WorkspaceResourceOperationEntry::new("other-operation".into()),
            );
            root.fail_workspace_split_admission(
                operation_id,
                &candidate,
                "fixture admission failure".into(),
                cx,
            );
            assert!(
                root.workspace_controller
                    .as_ref()
                    .unwrap()
                    .pending_resource_operation()
                    .is_none()
            );
            assert!(root.workspace_runtime.pane(&candidate).is_some());
            assert_eq!(
                root.workspace_resource_operations
                    .get(&candidate)
                    .expect("other owner remains")
                    .operation_id(),
                "other-operation"
            );
        });
    });
}
