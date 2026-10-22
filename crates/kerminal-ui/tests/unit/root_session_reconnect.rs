//! 显式 SSH 重连 controller 的身份、代次与取消收尾契约。
//!
//! @author kongweiguang

use kerminal_domain::{
    identity::{HostId, PaneId, SessionId, TabId},
    sessions::{
        SessionInfo, SessionLifecycle, SessionOwner, SessionTarget, SshSessionTarget, TerminalSize,
    },
};

use super::{
    ReconnectObservation, ReconnectResultState, SessionReconnectController, SessionReconnectState,
    matching_closed_target,
};
use crate::root::workspace_runtime::RuntimePanePhase;

/// 构造固定 pane/owner 绑定，确保单测使用与生产相同的 domain identity 类型。
fn identity() -> (TabId, PaneId, SessionId) {
    (TabId::new(), PaneId::new(), SessionId::new())
}

/// 创建结构化 SSH 目标，测试重连不退化成新 session 或本地 shell。
fn ssh_target() -> SessionTarget {
    SessionTarget::Ssh(SshSessionTarget {
        host_id: HostId::new(),
        working_directory: None,
        command: None,
    })
}

/// 构造带真实生命周期和 generation 的列表观察，不从 UI 文本推导 session state。
fn observation(
    tab_id: &TabId,
    pane_id: &PaneId,
    session_id: &SessionId,
    target: SessionTarget,
    lifecycle: SessionLifecycle,
    generation: u64,
) -> ReconnectObservation {
    ReconnectObservation {
        info: SessionInfo {
            session_id: session_id.clone(),
            target,
            owner: SessionOwner::Ui {
                tab_id: tab_id.clone(),
                pane_id: pane_id.clone(),
            },
            size: TerminalSize::default(),
            lifecycle,
            exit_status: None,
        },
        session_generation: generation,
    }
}

/// 把 service observation 注入 controller，模拟 lifecycle watcher 完成一次真实查询。
fn observe(
    controller: &mut SessionReconnectController,
    tab_id: &TabId,
    pane_id: &PaneId,
    session_id: &SessionId,
    target: SessionTarget,
    lifecycle: SessionLifecycle,
    generation: u64,
) {
    let owner = SessionOwner::Ui {
        tab_id: tab_id.clone(),
        pane_id: pane_id.clone(),
    };
    let ticket = controller.begin_refresh(pane_id, session_id.clone(), owner, target.clone());
    controller.finish_refresh(
        pane_id,
        ticket,
        Ok(observation(
            tab_id, pane_id, session_id, target, lifecycle, generation,
        )),
    );
}

/// 断开的 UI SSH pane 才生成带 exact binding 的显式重连动作。
#[test]
fn projection_requires_matching_ui_ssh_owner_and_disconnected_lifecycle() {
    let (tab_id, pane_id, session_id) = identity();
    let target = ssh_target();
    let mut controller = SessionReconnectController::default();
    observe(
        &mut controller,
        &tab_id,
        &pane_id,
        &session_id,
        target.clone(),
        SessionLifecycle::Exited,
        9,
    );

    let projection = controller.projection(
        &pane_id,
        &tab_id,
        &session_id,
        &target,
        RuntimePanePhase::Ready,
    );
    assert!(matches!(
        projection,
        Some(projection)
            if projection.binding.session_id == session_id
                && projection.binding.session_generation == 9
                && projection.state == SessionReconnectState::Available
    ));
    assert!(
        controller
            .projection(
                &pane_id,
                &tab_id,
                &session_id,
                &target,
                RuntimePanePhase::Closing,
            )
            .is_none()
    );

    let other_tab = TabId::new();
    assert!(
        controller
            .projection(
                &pane_id,
                &other_tab,
                &session_id,
                &target,
                RuntimePanePhase::Ready,
            )
            .is_none()
    );

    observe(
        &mut controller,
        &tab_id,
        &pane_id,
        &session_id,
        target.clone(),
        SessionLifecycle::Running,
        10,
    );
    assert!(
        controller
            .projection(
                &pane_id,
                &tab_id,
                &session_id,
                &target,
                RuntimePanePhase::Ready,
            )
            .is_none()
    );
}

/// 重启入口覆盖运行中和断开态，但 Starting/Closing/Closed 均不能生成操作绑定。
///
/// @author kongweiguang
#[test]
fn restart_projection_requires_a_supported_lifecycle() {
    let (tab_id, pane_id, session_id) = identity();
    let target = ssh_target();
    for lifecycle in [
        SessionLifecycle::Running,
        SessionLifecycle::Exited,
        SessionLifecycle::Failed,
    ] {
        let mut controller = SessionReconnectController::default();
        observe(
            &mut controller,
            &tab_id,
            &pane_id,
            &session_id,
            target.clone(),
            lifecycle,
            11,
        );
        let projection = controller.restart_projection(
            &pane_id,
            &tab_id,
            &session_id,
            &target,
            RuntimePanePhase::Ready,
        );
        assert!(matches!(
            projection,
            Some(projection)
                if projection.binding.session_id == session_id
                    && projection.binding.session_generation == 11
                    && !projection.busy
        ));
    }

    for lifecycle in [
        SessionLifecycle::Starting,
        SessionLifecycle::Closing,
        SessionLifecycle::Closed,
    ] {
        let mut controller = SessionReconnectController::default();
        observe(
            &mut controller,
            &tab_id,
            &pane_id,
            &session_id,
            target.clone(),
            lifecycle,
            11,
        );
        assert!(
            controller
                .restart_projection(
                    &pane_id,
                    &tab_id,
                    &session_id,
                    &target,
                    RuntimePanePhase::Ready,
                )
                .is_none()
        );
    }
}

/// 重启与重连共享 pane 操作锁；busy 投影持续使用发起时的 binding，防止双击和交叉请求。
///
/// @author kongweiguang
#[test]
fn restart_and_reconnect_share_a_busy_pane_lock() {
    let (tab_id, pane_id, session_id) = identity();
    let target = ssh_target();
    let mut controller = SessionReconnectController::default();
    observe(
        &mut controller,
        &tab_id,
        &pane_id,
        &session_id,
        target.clone(),
        SessionLifecycle::Running,
        4,
    );
    let binding = controller
        .restart_projection(
            &pane_id,
            &tab_id,
            &session_id,
            &target,
            RuntimePanePhase::Ready,
        )
        .expect("running pane 应提供重启 binding")
        .binding;
    let Some((attempt_id, _)) =
        controller.begin_restart_attempt(&pane_id, &tab_id, binding.clone(), target.clone())
    else {
        panic!("首个重启操作应被接受");
    };

    assert!(
        controller
            .begin_restart_attempt(&pane_id, &tab_id, binding.clone(), target.clone())
            .is_none()
    );
    assert!(
        controller
            .begin_attempt(&pane_id, &tab_id, binding.clone(), target.clone())
            .is_none()
    );
    assert!(matches!(
        controller.restart_projection(
            &pane_id,
            &tab_id,
            &session_id,
            &target,
            RuntimePanePhase::Ready,
        ),
        Some(projection) if projection.busy && projection.binding == binding
    ));

    assert!(controller.finish_attempt(
        attempt_id,
        Ok(observation(
            &tab_id,
            &pane_id,
            &session_id,
            target.clone(),
            SessionLifecycle::Running,
            5,
        )),
    ));
    assert!(matches!(
        controller.restart_projection(
            &pane_id,
            &tab_id,
            &session_id,
            &target,
            RuntimePanePhase::Ready,
        ),
        Some(projection) if !projection.busy && projection.binding.session_generation == 5
    ));
}

/// Restart ACK 即使状态仍为 Running，也必须验证 generation 严格前进才解除操作失败标记。
///
/// @author kongweiguang
#[test]
fn restart_rejects_running_ack_without_a_new_generation() {
    let (tab_id, pane_id, session_id) = identity();
    let target = ssh_target();
    let mut controller = SessionReconnectController::default();
    observe(
        &mut controller,
        &tab_id,
        &pane_id,
        &session_id,
        target.clone(),
        SessionLifecycle::Exited,
        8,
    );
    let binding = controller
        .restart_projection(
            &pane_id,
            &tab_id,
            &session_id,
            &target,
            RuntimePanePhase::Ready,
        )
        .expect("exited pane 也可以显式重启")
        .binding;
    let (attempt_id, _) = controller
        .begin_restart_attempt(&pane_id, &tab_id, binding, target.clone())
        .expect("重启应被接受");

    assert!(controller.finish_attempt(
        attempt_id,
        Ok(observation(
            &tab_id,
            &pane_id,
            &session_id,
            target,
            SessionLifecycle::Running,
            8,
        )),
    ));
    assert!(matches!(
        controller.panes.get(&pane_id).map(|state| &state.result),
        Some(ReconnectResultState::Failed(_))
    ));
}

/// 取消与 pane 替换只发 token；attempt owner 在 finish ACK 前留在 in-flight 表中。
///
/// @author kongweiguang
#[test]
fn cancellation_keeps_owner_until_ack_and_stale_finish_cannot_touch_replacement() {
    let (tab_id, pane_id, session_id) = identity();
    let target = ssh_target();
    let mut controller = SessionReconnectController::default();
    observe(
        &mut controller,
        &tab_id,
        &pane_id,
        &session_id,
        target.clone(),
        SessionLifecycle::Failed,
        2,
    );
    let binding = controller
        .projection(
            &pane_id,
            &tab_id,
            &session_id,
            &target,
            RuntimePanePhase::Ready,
        )
        .map(|projection| projection.binding);
    let Some(binding) = binding else {
        panic!("断开的 UI SSH pane 应可重连");
    };
    let Some((attempt_id, cancellation)) =
        controller.begin_attempt(&pane_id, &tab_id, binding.clone(), target.clone())
    else {
        panic!("第一个重连 attempt 应被接受");
    };

    assert!(
        controller
            .begin_attempt(&pane_id, &tab_id, binding.clone(), target.clone())
            .is_none()
    );
    assert!(controller.cancel_attempt(&pane_id, &binding, attempt_id));
    assert!(
        cancellation
            .requested
            .load(std::sync::atomic::Ordering::Acquire)
    );
    assert_eq!(controller.in_flight.len(), 1);
    assert!(matches!(
        controller.projection(
            &pane_id,
            &tab_id,
            &session_id,
            &target,
            RuntimePanePhase::Ready,
        ),
        Some(projection)
            if projection.state == SessionReconnectState::Cancelling { attempt_id }
    ));

    controller.cancel_for_pane(&pane_id);
    assert!(!controller.panes.contains_key(&pane_id));
    assert_eq!(controller.in_flight.len(), 1);

    let replacement_session = SessionId::new();
    observe(
        &mut controller,
        &tab_id,
        &pane_id,
        &replacement_session,
        target.clone(),
        SessionLifecycle::Exited,
        1,
    );
    assert!(
        controller
            .projection(
                &pane_id,
                &tab_id,
                &replacement_session,
                &target,
                RuntimePanePhase::Ready,
            )
            .is_none()
    );

    assert!(!controller.finish_attempt(
        attempt_id,
        Err(kerminal_application::AppError::cancelled("测试取消")),
    ));
    assert_eq!(controller.in_flight.len(), 0);
    assert!(matches!(
        controller.projection(
            &pane_id,
            &tab_id,
            &replacement_session,
            &target,
            RuntimePanePhase::Ready,
        ),
        Some(projection) if projection.state == SessionReconnectState::Available
    ));
}

/// session 查询或 generation 已过期时，迟到的 refresh 不得覆盖新 revision。
#[test]
fn stale_refresh_does_not_replace_newer_observation() {
    let (tab_id, pane_id, session_id) = identity();
    let target = ssh_target();
    let owner = SessionOwner::Ui {
        tab_id: tab_id.clone(),
        pane_id: pane_id.clone(),
    };
    let mut controller = SessionReconnectController::default();
    let old_ticket =
        controller.begin_refresh(&pane_id, session_id.clone(), owner.clone(), target.clone());
    let new_ticket = controller.begin_refresh(&pane_id, session_id.clone(), owner, target.clone());
    controller.finish_refresh(
        &pane_id,
        new_ticket,
        Ok(observation(
            &tab_id,
            &pane_id,
            &session_id,
            target.clone(),
            SessionLifecycle::Exited,
            5,
        )),
    );
    controller.finish_refresh(
        &pane_id,
        old_ticket,
        Ok(observation(
            &tab_id,
            &pane_id,
            &session_id,
            target.clone(),
            SessionLifecycle::Running,
            4,
        )),
    );

    let projection = controller.projection(
        &pane_id,
        &tab_id,
        &session_id,
        &target,
        RuntimePanePhase::Ready,
    );
    assert!(matches!(
        projection,
        Some(projection)
            if projection.binding.session_generation == 5
                && projection.state == SessionReconnectState::Available
    ));
}

/// 重连成功后必须换 generation、保留原 target；只有兼容的新 runtime 才能清除重连状态。
#[test]
fn completed_attempt_requires_same_session_and_new_running_generation() {
    let (tab_id, pane_id, session_id) = identity();
    let target = ssh_target();
    let mut controller = SessionReconnectController::default();
    observe(
        &mut controller,
        &tab_id,
        &pane_id,
        &session_id,
        target.clone(),
        SessionLifecycle::Exited,
        11,
    );
    let Some(binding) = controller
        .projection(
            &pane_id,
            &tab_id,
            &session_id,
            &target,
            RuntimePanePhase::Ready,
        )
        .map(|projection| projection.binding)
    else {
        panic!("测试前应存在可重连投影");
    };
    let Some((attempt_id, _)) =
        controller.begin_attempt(&pane_id, &tab_id, binding, target.clone())
    else {
        panic!("测试重连应能启动");
    };
    let result = observation(
        &tab_id,
        &pane_id,
        &session_id,
        target.clone(),
        SessionLifecycle::Running,
        12,
    );
    assert!(controller.finish_attempt(attempt_id, Ok(result)));
    assert!(
        controller
            .projection(
                &pane_id,
                &tab_id,
                &session_id,
                &target,
                RuntimePanePhase::Ready,
            )
            .is_none()
    );

    let (tab_id, pane_id, session_id) = identity();
    let mut same_generation_controller = SessionReconnectController::default();
    observe(
        &mut same_generation_controller,
        &tab_id,
        &pane_id,
        &session_id,
        target.clone(),
        SessionLifecycle::Exited,
        12,
    );
    let Some(binding) = same_generation_controller
        .projection(
            &pane_id,
            &tab_id,
            &session_id,
            &target,
            RuntimePanePhase::Ready,
        )
        .map(|projection| projection.binding)
    else {
        panic!("测试前应存在可重连投影");
    };
    let Some((attempt_id, _)) =
        same_generation_controller.begin_attempt(&pane_id, &tab_id, binding, target.clone())
    else {
        panic!("测试重连应能启动");
    };
    let same_generation_result = observation(
        &tab_id,
        &pane_id,
        &session_id,
        target.clone(),
        SessionLifecycle::Running,
        12,
    );
    assert!(same_generation_controller.finish_attempt(attempt_id, Ok(same_generation_result),));
    assert!(matches!(
        same_generation_controller.projection(
            &pane_id,
            &tab_id,
            &session_id,
            &target,
            RuntimePanePhase::Ready,
        ),
        Some(projection)
            if matches!(projection.state, SessionReconnectState::Failed { .. })
    ));
}

/// pane 状态默认创建后不携带任何未经 service 确认的重连目标或 lifecycle。
#[test]
fn empty_controller_does_not_invent_a_reconnect_observation() {
    let (tab_id, pane_id, session_id) = identity();
    let controller = SessionReconnectController::default();
    assert!(
        controller
            .projection(
                &pane_id,
                &tab_id,
                &session_id,
                &ssh_target(),
                RuntimePanePhase::Ready,
            )
            .is_none()
    );
}

/// 面板 accessor 只读出精确 owner、target 和 generation 的最新 observation。
///
/// @author kongweiguang
#[test]
fn panel_observation_requires_exact_identity_and_no_in_flight_reconnect() {
    let (tab_id, pane_id, session_id) = identity();
    let target = ssh_target();
    let mut controller = SessionReconnectController::default();
    observe(
        &mut controller,
        &tab_id,
        &pane_id,
        &session_id,
        target.clone(),
        SessionLifecycle::Running,
        17,
    );

    let panel = controller
        .panel_observation(&pane_id, &tab_id, &session_id, &target)
        .expect("当前 pane 的精确观察应可供只读面板使用");
    assert_eq!(panel.binding.session_id, session_id);
    assert_eq!(panel.binding.session_generation, 17);
    assert_eq!(panel.target, target);
    assert_eq!(panel.lifecycle, SessionLifecycle::Running);

    let other_tab = TabId::new();
    assert!(
        controller
            .panel_observation(&pane_id, &other_tab, &session_id, &ssh_target())
            .is_none()
    );
    assert!(
        controller
            .panel_observation(&pane_id, &tab_id, &SessionId::new(), &ssh_target())
            .is_none()
    );
}

/// 重连正在处理时，面板不得把尚未收敛的旧 lifecycle/generation 当成当前状态。
///
/// @author kongweiguang
#[test]
fn panel_observation_is_unavailable_while_reconnect_owns_the_pane() {
    let (tab_id, pane_id, session_id) = identity();
    let target = ssh_target();
    let mut controller = SessionReconnectController::default();
    observe(
        &mut controller,
        &tab_id,
        &pane_id,
        &session_id,
        target.clone(),
        SessionLifecycle::Exited,
        17,
    );
    let binding = controller
        .projection(
            &pane_id,
            &tab_id,
            &session_id,
            &target,
            RuntimePanePhase::Ready,
        )
        .expect("退出的同一终端应存在重连投影")
        .binding;
    controller
        .begin_attempt(&pane_id, &tab_id, binding, target.clone())
        .expect("首次重连应占有 pane");

    assert!(
        controller
            .panel_observation(&pane_id, &tab_id, &session_id, &target)
            .is_none()
    );
}

/// 关闭回执必须先匹配 runtime 当前 session；其他 session 的迟到事件不撤换既有观察。
///
/// @author kongweiguang
#[test]
fn closed_receipt_for_another_session_leaves_panel_observation_unchanged() {
    let (tab_id, pane_id, session_id) = identity();
    let target = ssh_target();
    let mut controller = SessionReconnectController::default();
    observe(
        &mut controller,
        &tab_id,
        &pane_id,
        &session_id,
        target.clone(),
        SessionLifecycle::Running,
        17,
    );

    let before = controller
        .panel_observation(&pane_id, &tab_id, &session_id, &target)
        .expect("当前 pane 应保留可信观察");
    assert!(
        matching_closed_target(
            &tab_id,
            Some(&session_id),
            Some(&target),
            RuntimePanePhase::Ready,
            &tab_id,
            &SessionId::new(),
        )
        .is_none()
    );
    let after = controller
        .panel_observation(&pane_id, &tab_id, &session_id, &target)
        .expect("不匹配的关闭回执不能清除当前观察");
    assert_eq!(after, before);
}

/// 精确关闭会使旧 refresh incarnation 失效，取消重连 owner，并保留原 binding 标为终态。
///
/// @author kongweiguang
#[test]
fn exact_closed_receipt_cancels_stale_work_and_preserves_readonly_identity() {
    let (tab_id, pane_id, session_id) = identity();
    let target = ssh_target();
    let mut controller = SessionReconnectController::default();
    observe(
        &mut controller,
        &tab_id,
        &pane_id,
        &session_id,
        target.clone(),
        SessionLifecycle::Exited,
        17,
    );
    let binding = controller
        .projection(
            &pane_id,
            &tab_id,
            &session_id,
            &target,
            RuntimePanePhase::Ready,
        )
        .expect("退出 pane 应有原会话 binding")
        .binding;
    let (attempt_id, cancellation) = controller
        .begin_attempt(&pane_id, &tab_id, binding.clone(), target.clone())
        .expect("重连 attempt 应持有原 pane");
    let stale_refresh = controller.begin_refresh(
        &pane_id,
        session_id.clone(),
        binding.owner.clone(),
        target.clone(),
    );

    assert!(
        matching_closed_target(
            &tab_id,
            Some(&session_id),
            Some(&target),
            RuntimePanePhase::Ready,
            &tab_id,
            &session_id,
        )
        .is_some()
    );
    controller.record_closed_observation(&tab_id, &pane_id, &session_id, &target);
    assert!(
        cancellation
            .requested
            .load(std::sync::atomic::Ordering::Acquire)
    );

    controller.finish_refresh(
        &pane_id,
        stale_refresh,
        Ok(observation(
            &tab_id,
            &pane_id,
            &session_id,
            target.clone(),
            SessionLifecycle::Running,
            18,
        )),
    );
    let panel = controller
        .panel_observation(&pane_id, &tab_id, &session_id, &target)
        .expect("Closed receipt 应可在取消 ACK 前驱动终态只读提示");
    assert_eq!(panel.binding, binding);
    assert_eq!(panel.lifecycle, SessionLifecycle::Closed);

    assert!(
        !controller.finish_attempt(
            attempt_id,
            Ok(observation(
                &tab_id,
                &pane_id,
                &session_id,
                target.clone(),
                SessionLifecycle::Running,
                18,
            )),
        ),
        "旧 attempt ACK 不能改写新的 Closed incarnation"
    );
    assert_eq!(
        controller
            .panel_observation(&pane_id, &tab_id, &session_id, &target)
            .expect("旧 ACK 后 Closed observation 仍应保留")
            .lifecycle,
        SessionLifecycle::Closed
    );
}
