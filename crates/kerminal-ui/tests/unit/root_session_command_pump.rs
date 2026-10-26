//! 真实 session FIFO 回执只更新仍有效的 owner，关闭与换代不污染全局错误。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString};

use crate::root::{
    session_command_pump::{
        SessionCommandBindingSource, SessionCommandReceipt, accept_session_command_result,
        allow_canvas_resize_after_failure, focus_failure_is_proven_ended,
        input_returns_viewport_to_bottom, observation_proves_session_not_running,
        session_command_owner, session_command_view_owner, session_work_binding_is_current,
    },
    session_queue::{
        InputOrigin, SessionCommandQueues, SessionQueueLimits, SessionWork, UiInputWork,
    },
    session_reconnect::SessionPanelObservation,
    workspace_runtime::WorkspaceRuntime,
};
use gpui_kit::SharedString;
use kerminal_application::{
    AppError,
    sessions::{UiInputProjection, UiSessionBinding, UiTerminalInputRequest},
    workspace::{
        WorkspaceLocalMachineScope, WorkspaceMachineStatus, WorkspacePaneMode,
        WorkspaceSessionPane, WorkspaceSessionSnapshot, WorkspaceSessionTab, WorkspaceTargetRef,
    },
};
use kerminal_domain::{
    identity::{AgentSessionId, PaneId, SessionId, TabId},
    sessions::{LocalSessionTarget, SessionLifecycle, SessionOwner, SessionTarget, TerminalSize},
    workspace::TerminalLayoutNode,
};
use kerminal_terminal::input::{
    InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyEvent,
    TerminalMouseAction, TerminalMouseButton, TerminalMouseEvent,
};

/// 回到底部只绑定真实 shell 写入事件，焦点与应用鼠标报告继续保持当前阅读位置。
///
/// @author kongweiguang
#[test]
fn only_shell_input_returns_the_viewport_to_bottom() {
    let key = TerminalKeyEvent {
        key: TerminalKey::Enter,
        phase: KeyPhase::Press,
        modifiers: InputModifiers::default(),
    };
    let mouse = TerminalMouseEvent {
        button: TerminalMouseButton::WheelUp,
        action: TerminalMouseAction::Scroll,
        button_down: false,
        column: 0,
        row: 0,
        modifiers: InputModifiers::default(),
    };

    assert!(input_returns_viewport_to_bottom(&TerminalInputEvent::Key(
        key
    )));
    assert!(input_returns_viewport_to_bottom(&TerminalInputEvent::Text(
        "命令".to_owned()
    )));
    assert!(input_returns_viewport_to_bottom(
        &TerminalInputEvent::Paste("粘贴".to_owned())
    ));
    assert!(!input_returns_viewport_to_bottom(
        &TerminalInputEvent::Mouse(mouse)
    ));
    assert!(!input_returns_viewport_to_bottom(
        &TerminalInputEvent::Focus(true)
    ));
}

/// 失败后的重复绘制不产生新 RPC；更改布局、撤销绑定或换 session 后才能显式重新尝试。
///
/// @author kongweiguang
#[test]
fn failed_resize_is_not_retried_by_repaint_and_does_not_block_other_sessions() {
    let failed_session = SessionId::new();
    let other_session = SessionId::new();
    let failed = TerminalSize::new(80, 24, 800, 480).unwrap();
    let changed = TerminalSize::new(81, 24, 810, 480).unwrap();
    let mut failures = std::collections::HashMap::from([(failed_session.clone(), failed)]);
    for _ in 0..100 {
        assert!(!allow_canvas_resize_after_failure(
            &mut failures,
            &failed_session,
            failed,
        ));
    }
    assert!(allow_canvas_resize_after_failure(
        &mut failures,
        &other_session,
        failed
    ));
    assert_eq!(failures.len(), 1);
    assert!(allow_canvas_resize_after_failure(
        &mut failures,
        &failed_session,
        changed
    ));
    assert!(failures.is_empty());
    assert!(allow_canvas_resize_after_failure(
        &mut failures,
        &failed_session,
        failed
    ));
    failures.insert(failed_session.clone(), failed);
    failures.remove(&failed_session);
    assert!(allow_canvas_resize_after_failure(
        &mut failures,
        &failed_session,
        failed
    ));
}

/// 仅精确同代的结束观察抑制尺寸失败提示；观测缺失、错代及仍运行都保留真实错误。
///
/// @author kongweiguang
#[test]
fn resize_failure_is_suppressed_only_for_exact_observed_ended_binding() {
    let fixture = fixture();
    let observation = |lifecycle| SessionPanelObservation {
        binding: fixture.binding.clone(),
        target: target(),
        lifecycle,
    };

    assert!(!observation_proves_session_not_running(
        &fixture.binding,
        None,
    ));
    assert!(!observation_proves_session_not_running(
        &fixture.binding,
        Some(&observation(SessionLifecycle::Running)),
    ));
    for lifecycle in [
        SessionLifecycle::Starting,
        SessionLifecycle::Closing,
        SessionLifecycle::Closed,
        SessionLifecycle::Exited,
        SessionLifecycle::Failed,
    ] {
        assert!(observation_proves_session_not_running(
            &fixture.binding,
            Some(&observation(lifecycle)),
        ));
    }

    let mut stale_binding = fixture.binding.clone();
    stale_binding.session_generation += 1;
    assert!(!observation_proves_session_not_running(
        &stale_binding,
        Some(&observation(SessionLifecycle::Exited)),
    ));
}

/// 迟到 Focus 失败只在同一会话已权威结束时静默；真实输入、运行中及错代错误仍可见。
///
/// @author kongweiguang
#[test]
fn focus_failure_is_suppressed_only_for_exact_observed_ended_binding() {
    let fixture = fixture();
    let target = target();
    let request = |binding, event| UiTerminalInputRequest {
        binding,
        expected_context: None,
        projection: UiInputProjection::Invalidates,
        event,
    };
    let observation = |binding, target, lifecycle| SessionPanelObservation {
        binding,
        target,
        lifecycle,
    };

    for event in [
        TerminalInputEvent::Focus(false),
        TerminalInputEvent::Focus(true),
    ] {
        let request = request(fixture.binding.clone(), event);
        for lifecycle in [
            SessionLifecycle::Starting,
            SessionLifecycle::Closing,
            SessionLifecycle::Exited,
            SessionLifecycle::Failed,
            SessionLifecycle::Closed,
        ] {
            let observed = observation(fixture.binding.clone(), target.clone(), lifecycle);
            assert!(focus_failure_is_proven_ended(
                &request,
                Some(&fixture.binding),
                &target,
                Some(&observed),
            ));
        }
        let running = observation(
            fixture.binding.clone(),
            target.clone(),
            SessionLifecycle::Running,
        );
        assert!(!focus_failure_is_proven_ended(
            &request,
            Some(&fixture.binding),
            &target,
            Some(&running),
        ));
        assert!(!focus_failure_is_proven_ended(
            &request,
            Some(&fixture.binding),
            &target,
            None,
        ));

        let mut stale_generation = fixture.binding.clone();
        stale_generation.session_generation += 1;
        let stale_generation_observation =
            observation(stale_generation, target.clone(), SessionLifecycle::Exited);
        assert!(!focus_failure_is_proven_ended(
            &request,
            Some(&fixture.binding),
            &target,
            Some(&stale_generation_observation),
        ));

        let mut stale_owner = fixture.binding.clone();
        stale_owner.owner = SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        };
        let stale_owner_observation =
            observation(stale_owner, target.clone(), SessionLifecycle::Exited);
        assert!(!focus_failure_is_proven_ended(
            &request,
            Some(&fixture.binding),
            &target,
            Some(&stale_owner_observation),
        ));

        let mut different_target = target.clone();
        if let SessionTarget::Local(local) = &mut different_target {
            local.program = OsString::from("different-shell");
        }
        let different_target_observation = observation(
            fixture.binding.clone(),
            different_target,
            SessionLifecycle::Exited,
        );
        assert!(!focus_failure_is_proven_ended(
            &request,
            Some(&fixture.binding),
            &target,
            Some(&different_target_observation),
        ));
    }

    let ended = observation(
        fixture.binding.clone(),
        target.clone(),
        SessionLifecycle::Exited,
    );
    for event in [
        TerminalInputEvent::Text("x".to_owned()),
        TerminalInputEvent::Key(TerminalKeyEvent {
            key: TerminalKey::Enter,
            phase: KeyPhase::Press,
            modifiers: InputModifiers::default(),
        }),
    ] {
        let request = request(fixture.binding.clone(), event);
        assert!(!focus_failure_is_proven_ended(
            &request,
            Some(&fixture.binding),
            &target,
            Some(&ended),
        ));
    }

    let stale_request = request(
        {
            let mut binding = fixture.binding.clone();
            binding.session_generation += 1;
            binding
        },
        TerminalInputEvent::Focus(false),
    );
    assert!(!focus_failure_is_proven_ended(
        &stale_request,
        Some(&fixture.binding),
        &target,
        Some(&ended),
    ));
}

/// 固定真实 Runtime pane/session 身份；controller 的 active Tab 与物理焦点不参与回执权限。
struct PumpFixture {
    snapshot: WorkspaceSessionSnapshot,
    runtime: WorkspaceRuntime,
    session_id: SessionId,
    pane_id: PaneId,
    binding: UiSessionBinding,
    queues: SessionCommandQueues,
}

/// 构造一份已就绪的持久 pane，再用生产 Runtime 安装 session，不以 bool 模拟关闭状态。
///
/// @author kongweiguang
fn fixture() -> PumpFixture {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let session_id = SessionId::new();
    let snapshot = WorkspaceSessionSnapshot {
        active_tab_id: Some(tab_id.clone()),
        focused_pane_id: Some(pane_id.clone()),
        selected_machine_id: None,
        terminal_tabs: vec![WorkspaceSessionTab::Terminal {
            id: tab_id.clone(),
            tab_group_id: None,
            title: "Local".to_owned(),
            machine_id: "local".to_owned(),
            layout: TerminalLayoutNode::pane(pane_id.clone()),
        }],
        terminal_panes: vec![WorkspaceSessionPane {
            id: pane_id.clone(),
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
        }],
        tab_groups: BTreeMap::new(),
    };
    let mut runtime = WorkspaceRuntime::from_snapshot(&snapshot).expect("valid workspace fixture");
    runtime
        .install_session(
            &pane_id,
            session_id.clone(),
            target(),
            TerminalSize::default(),
        )
        .expect("real Runtime holds the session owner");
    PumpFixture {
        snapshot,
        runtime,
        session_id: session_id.clone(),
        pane_id: pane_id.clone(),
        binding: UiSessionBinding {
            session_id,
            owner: SessionOwner::Ui { tab_id, pane_id },
            session_generation: 7,
        },
        queues: SessionCommandQueues::with_limits(SessionQueueLimits::default()),
    }
}

/// 目标仅供 Runtime 身份校验，不创建真实 PTY 或访问用户配置。
///
/// @author kongweiguang
fn target() -> SessionTarget {
    SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("fixture-shell"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    })
}

/// 与生产 pump 相同地从 FIFO 取项后捕获 owner/binding，保留 in-flight 槽直到回执处理。
///
/// @author kongweiguang
fn dispatch(fixture: &mut PumpFixture, work: SessionWork) -> SessionCommandReceipt {
    fixture
        .queues
        .enqueue(fixture.session_id.clone(), work)
        .expect("fixture has queue budget");
    let ready = fixture
        .queues
        .take_ready()
        .expect("FIFO dispatches the item");
    let owner = session_command_owner(&fixture.runtime, &ready.session_id, None, None)
        .expect("dispatch captures a live Runtime owner");
    SessionCommandReceipt::capture(&ready, owner, Some(&fixture.binding))
}

/// 退出后的绝对滚动只需精确只读 view binding；任何 PTY 输入仍必须持有 live handler。
///
/// @author kongweiguang
#[test]
fn history_scroll_uses_view_binding_without_requiring_input_handler() {
    let binding = UiSessionBinding {
        session_id: SessionId::new(),
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
        session_generation: 4,
    };
    let scroll = SessionWork::ScrollTo {
        binding: binding.clone(),
        offset: 17,
    };
    assert!(session_work_binding_is_current(
        &scroll,
        None,
        Some(&binding)
    ));
    assert!(!session_work_binding_is_current(&scroll, None, None));

    let mut replacement = binding.clone();
    replacement.session_generation += 1;
    assert!(!session_work_binding_is_current(
        &scroll,
        None,
        Some(&replacement)
    ));

    let mut queues = SessionCommandQueues::with_limits(SessionQueueLimits::default());
    queues
        .enqueue(binding.session_id.clone(), scroll)
        .expect("history scroll enters the same session FIFO");
    let ready = queues.take_ready().expect("history scroll becomes ready");
    let receipt = SessionCommandReceipt::capture(&ready, binding.owner.clone(), None);
    assert_eq!(
        receipt.binding_source,
        SessionCommandBindingSource::ReadOnlyView
    );
    assert_eq!(receipt.binding, Some(binding.clone()));

    let write = SessionWork::Input {
        event: TerminalInputEvent::Text("stale".to_owned()),
        origin: InputOrigin::Direct,
    };
    assert!(!session_work_binding_is_current(
        &write,
        None,
        Some(&binding)
    ));
    assert!(session_work_binding_is_current(
        &write,
        Some(&binding),
        Some(&binding)
    ));
}

/// A failed close restores only read-only ownership; even a context-invalidating input must require a live handler.
/// Diagnostic correlation stays absent so this ownership gate does not depend on the timing registry.
///
/// @author kongweiguang
#[test]
fn close_failed_ticket_admits_history_work_but_not_input_or_live_resize() {
    let mut fixture = fixture();
    assert!(
        fixture
            .runtime
            .retire_history_binding(&fixture.pane_id, fixture.binding.clone())
    );
    fixture.runtime.begin_close(&fixture.pane_id).unwrap();
    assert!(
        fixture
            .runtime
            .mark_close_failed(&fixture.pane_id, &fixture.session_id)
            .unwrap()
    );

    assert!(session_command_owner(&fixture.runtime, &fixture.session_id, None, None).is_none());
    assert_eq!(
        session_command_view_owner(&fixture.runtime, &fixture.session_id, None, None),
        Some(fixture.binding.owner.clone())
    );
    assert!(session_work_binding_is_current(
        &SessionWork::ScrollTo {
            binding: fixture.binding.clone(),
            offset: 4
        },
        None,
        Some(&fixture.binding),
    ));
    assert!(!session_work_binding_is_current(
        &SessionWork::UiInput(Box::new(UiInputWork {
            request: UiTerminalInputRequest {
                binding: fixture.binding.clone(),
                event: TerminalInputEvent::Text("must not write".to_owned()),
                projection: UiInputProjection::Invalidates,
                expected_context: None,
            },
            acceptance: None,
            command_line_ticket: None,
            origin: InputOrigin::Direct,
            wheel_input: false,
            #[cfg(feature = "acceptance-metrics")]
            echo_probe_id: None,
        })),
        None,
        Some(&fixture.binding),
    ));
    assert!(!session_work_binding_is_current(
        &SessionWork::Resize {
            binding: fixture.binding.clone(),
            size: TerminalSize::default()
        },
        None,
        Some(&fixture.binding),
    ));
    let mut stale = fixture.binding.clone();
    stale.session_generation += 1;
    assert!(
        !fixture
            .runtime
            .close_failed_history_binding_matches(&fixture.pane_id, &stale)
    );
}

/// 历史网格重排使用只读 binding；运行态 PTY resize 必须同时保持 handler 与 view 同代。
///
/// @author kongweiguang
#[test]
fn history_resize_receipts_use_readonly_binding_and_live_resize_requires_both_bindings() {
    let binding = UiSessionBinding {
        session_id: SessionId::new(),
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
        session_generation: 8,
    };
    let history_resize = SessionWork::HistoryResize {
        binding: binding.clone(),
        size: TerminalSize::new(100, 30, 1000, 600).unwrap(),
    };
    assert!(session_work_binding_is_current(
        &history_resize,
        None,
        Some(&binding)
    ));

    let live_resize = SessionWork::Resize {
        binding: binding.clone(),
        size: TerminalSize::new(100, 30, 1000, 600).unwrap(),
    };
    assert!(session_work_binding_is_current(
        &live_resize,
        Some(&binding),
        Some(&binding)
    ));
    assert!(!session_work_binding_is_current(
        &live_resize,
        Some(&binding),
        None
    ));

    let mut queues = SessionCommandQueues::with_limits(SessionQueueLimits::default());
    queues
        .enqueue(binding.session_id.clone(), history_resize)
        .expect("history resize shares the session FIFO");
    let ready = queues.take_ready().unwrap();
    let receipt = SessionCommandReceipt::capture(&ready, binding.owner.clone(), None);
    assert_eq!(
        receipt.binding_source,
        SessionCommandBindingSource::ReadOnlyView
    );
    assert_eq!(receipt.binding, Some(binding.clone()));
    assert!(queues.complete(&binding.session_id));

    let selection_scroll = SessionWork::SelectionScroll {
        binding: binding.clone(),
        offset: 9,
        gesture_id: 21,
        request_id: 6,
    };
    assert!(session_work_binding_is_current(
        &selection_scroll,
        None,
        Some(&binding)
    ));
    queues
        .enqueue(binding.session_id.clone(), selection_scroll)
        .expect("selection scroll shares the exact readonly FIFO");
    let ready = queues.take_ready().unwrap();
    let selection_receipt = SessionCommandReceipt::capture(&ready, binding.owner.clone(), None);
    assert_eq!(
        selection_receipt.binding_source,
        SessionCommandBindingSource::ReadOnlyView
    );
    let gesture_receipt = selection_receipt
        .selection_scroll
        .as_ref()
        .expect("edge scroll retains gesture receipt identity");
    assert_eq!(gesture_receipt.binding, binding);
    assert_eq!(gesture_receipt.gesture_id, 21);
    assert_eq!(gesture_receipt.request_id, 6);
    assert!(
        accept_session_command_result(
            &selection_receipt,
            Some(selection_receipt.owner.clone()),
            Some(&binding),
            Ok(None),
        )
        .is_some()
    );
    let stale_binding = UiSessionBinding {
        session_id: binding.session_id.clone(),
        owner: binding.owner.clone(),
        session_generation: binding.session_generation + 1,
    };
    assert!(
        accept_session_command_result(
            &selection_receipt,
            Some(selection_receipt.owner.clone()),
            Some(&stale_binding),
            Ok(None),
        )
        .is_none()
    );
}

/// 关闭期间和移除后的迟到 Focus/Resize 结果不写状态、不提交 ACK，失败的 close 状态仍保留。
///
/// @author kongweiguang
#[test]
fn close_retires_in_flight_focus_and_resize_receipts_before_and_after_removal() {
    for is_resize in [false, true] {
        let mut fixture = fixture();
        let work = if is_resize {
            SessionWork::Resize {
                binding: fixture.binding.clone(),
                size: TerminalSize::default(),
            }
        } else {
            SessionWork::Input {
                event: TerminalInputEvent::Focus(false),
                origin: InputOrigin::Direct,
            }
        };
        let receipt = dispatch(&mut fixture, work);
        fixture.runtime.begin_close(&fixture.pane_id).unwrap();
        assert!(session_command_owner(&fixture.runtime, &fixture.session_id, None, None).is_none());
        assert!(
            accept_session_command_result(
                &receipt,
                session_command_owner(&fixture.runtime, &receipt.session_id, None, None),
                None,
                Err(AppError::not_found("终端会话不存在")),
            )
            .is_none()
        );
        assert!(accept_session_command_result(&receipt, None, None, Ok(None)).is_none());

        fixture
            .runtime
            .mark_close_failed(&fixture.pane_id, &fixture.session_id)
            .unwrap();
        fixture
            .runtime
            .set_status(
                &fixture.pane_id,
                Some(SharedString::from("real close failed")),
            )
            .unwrap();
        assert!(session_command_owner(&fixture.runtime, &fixture.session_id, None, None).is_none());
        assert_eq!(
            fixture
                .runtime
                .pane(&fixture.pane_id)
                .unwrap()
                .status
                .as_deref(),
            Some("real close failed")
        );
        fixture.runtime.begin_close(&fixture.pane_id).unwrap();
        fixture
            .runtime
            .remove_closed_session(&fixture.pane_id, &fixture.session_id)
            .unwrap();
        assert!(
            session_command_owner(&fixture.runtime, &fixture.session_id, None, None).is_none(),
            "retired IDs cannot be re-enqueued by late focus/resize"
        );
        assert!(
            accept_session_command_result(
                &receipt,
                session_command_owner(&fixture.runtime, &receipt.session_id, None, None),
                None,
                Err(AppError::not_found("终端会话不存在")),
            )
            .is_none()
        );
        fixture.queues.complete(&fixture.session_id);
        assert!(fixture.queues.take_ready().is_none());
    }
}

/// 同一 pane 的新 SessionId 和同一 SessionId 的新 binding 均不能接收旧控制项回执。
///
/// @author kongweiguang
#[test]
fn replaced_session_or_binding_rejects_old_pump_result() {
    let mut fixture = fixture();
    let initial_binding = fixture.binding.clone();
    let receipt = dispatch(
        &mut fixture,
        SessionWork::Resize {
            binding: initial_binding,
            size: TerminalSize::default(),
        },
    );
    let mut reconnected = fixture.binding.clone();
    reconnected.session_generation += 1;
    assert!(
        accept_session_command_result(
            &receipt,
            session_command_owner(&fixture.runtime, &receipt.session_id, None, None),
            Some(&reconnected),
            Err(AppError::not_found("old generation disappeared")),
        )
        .is_none()
    );

    fixture.runtime.begin_close(&fixture.pane_id).unwrap();
    fixture
        .runtime
        .remove_closed_session(&fixture.pane_id, &fixture.session_id)
        .unwrap();
    fixture.runtime = WorkspaceRuntime::from_snapshot(&fixture.snapshot).unwrap();
    let replacement_id = SessionId::new();
    fixture
        .runtime
        .install_session(
            &fixture.pane_id,
            replacement_id.clone(),
            target(),
            TerminalSize::default(),
        )
        .unwrap();
    let mut replacement_binding = fixture.binding.clone();
    replacement_binding.session_id = replacement_id.clone();
    assert!(
        accept_session_command_result(
            &receipt,
            session_command_owner(&fixture.runtime, &receipt.session_id, None, None),
            Some(&replacement_binding),
            Err(AppError::not_found("old session disappeared")),
        )
        .is_none()
    );
    assert_eq!(
        fixture.runtime.session_for_pane(&fixture.pane_id),
        Some(&replacement_id)
    );
    fixture.queues.complete(&fixture.session_id);
}

/// 仍存活的 owner 即使没有活动 Tab/焦点也保留原始错误，包含 NotFound，避免按码静默吞错。
///
/// @author kongweiguang
#[test]
fn live_owner_keeps_real_errors_without_any_focus_requirement() {
    let mut fixture = fixture();
    let receipt = dispatch(
        &mut fixture,
        SessionWork::Input {
            event: TerminalInputEvent::Focus(false),
            origin: InputOrigin::Direct,
        },
    );
    fixture.snapshot.active_tab_id = None;
    fixture.snapshot.focused_pane_id = None;
    for error in [
        AppError::not_found("live owner lookup failed"),
        AppError::infrastructure("native write failed"),
    ] {
        let expected_code = error.public_code();
        let expected_message = error.public_message().to_owned();
        let result = accept_session_command_result(
            &receipt,
            session_command_owner(&fixture.runtime, &receipt.session_id, None, None),
            Some(&fixture.binding),
            Err(error),
        )
        .expect("live owner result must reach the existing error display branch");
        let error = result.expect_err("the original failure is retained");
        assert_eq!(error.public_code(), expected_code);
        assert_eq!(error.public_message(), expected_message);
    }
    fixture.queues.complete(&fixture.session_id);
}

/// Agent 切换只使旧视图回执失效；门禁不关闭或取消仍存活的后台 Agent 会话。
///
/// @author kongweiguang
#[test]
fn agent_view_binding_change_rejects_only_its_old_receipt() {
    let runtime = WorkspaceRuntime::default();
    let agent_id = AgentSessionId::new();
    let session_id = SessionId::new();
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: SessionOwner::Agent {
            agent_session_id: agent_id.clone(),
        },
        session_generation: 3,
    };
    let mut queues = SessionCommandQueues::with_limits(SessionQueueLimits::default());
    queues
        .enqueue(
            session_id.clone(),
            SessionWork::Resize {
                binding: binding.clone(),
                size: TerminalSize::default(),
            },
        )
        .unwrap();
    let ready = queues.take_ready().unwrap();
    let receipt = SessionCommandReceipt::capture(&ready, binding.owner.clone(), Some(&binding));
    let live_owner =
        session_command_owner(&runtime, &session_id, Some(&agent_id), Some(&session_id));
    assert!(
        accept_session_command_result(
            &receipt,
            live_owner,
            Some(&binding),
            Err(AppError::infrastructure("agent resize failed"))
        )
        .unwrap()
        .is_err()
    );
    let replacement_agent = AgentSessionId::new();
    let changed_owner = session_command_owner(
        &runtime,
        &session_id,
        Some(&replacement_agent),
        Some(&session_id),
    );
    assert!(
        accept_session_command_result(
            &receipt,
            changed_owner,
            Some(&binding),
            Err(AppError::not_found("old view result"))
        )
        .is_none()
    );
    queues.complete(&session_id);
}
