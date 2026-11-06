//! Local 与远程 terminal transport 共用原 pane 的手动恢复契约。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

use kerminal_domain::{
    files::ContainerRuntime,
    identity::{HostId, PaneId, SessionId, TabId},
    sessions::{
        ContainerSessionTarget, LocalSessionTarget, SerialSessionTarget, SessionInfo,
        SessionLifecycle, SessionOwner, SessionTarget, SshSessionTarget, TelnetSessionTarget,
        TerminalSize,
    },
};

use super::{ReconnectObservation, ReconnectResultState, SessionReconnectController};
use crate::root::workspace_runtime::RuntimePanePhase;

/// 构造桌面 composition 注册的所有 terminal target，防止手动重连只覆盖 SSH。
///
/// @author kongweiguang
fn manual_reconnect_targets() -> Vec<SessionTarget> {
    vec![
        SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("cmd.exe"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        SessionTarget::Ssh(SshSessionTarget {
            host_id: HostId::new(),
            working_directory: None,
            command: None,
        }),
        SessionTarget::Telnet(TelnetSessionTarget {
            host: "localhost".into(),
            port: 23,
        }),
        SessionTarget::Serial(SerialSessionTarget::new(PathBuf::from("COM1"))),
        SessionTarget::Container(ContainerSessionTarget {
            host_id: None,
            runtime: ContainerRuntime::Docker,
            container_id: "container-id".into(),
            command: Vec::new(),
        }),
    ]
}

/// 为控制器测试创建互不复用的 pane、Tab 和 session 身份。
///
/// @author kongweiguang
fn identity() -> (TabId, PaneId, SessionId) {
    (TabId::new(), PaneId::new(), SessionId::new())
}

/// 构造带真实 owner、target、lifecycle 和 runtime generation 的 service 观察值。
///
/// @author kongweiguang
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

/// 将测试观察通过与生产相同的刷新票据送入控制器，避免直接伪造内部缓存。
///
/// @author kongweiguang
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

/// 所有已注册 transport 都按原身份暴露同一重新连接入口；未确认或关闭的 session 不显示入口。
///
/// @author kongweiguang
#[test]
fn supported_runtime_targets_offer_one_reconnect_action() {
    for target in manual_reconnect_targets() {
        for lifecycle in [
            SessionLifecycle::Running,
            SessionLifecycle::Exited,
            SessionLifecycle::Failed,
        ] {
            let (tab_id, pane_id, session_id) = identity();
            let mut controller = SessionReconnectController::default();
            observe(
                &mut controller,
                &tab_id,
                &pane_id,
                &session_id,
                target.clone(),
                lifecycle,
                4,
            );

            let projection = controller.action_projection(
                &pane_id,
                &tab_id,
                &session_id,
                &target,
                RuntimePanePhase::Ready,
            );
            let Some(projection) = projection else {
                panic!("受支持的终端目标应生成重新连接入口：{target:?}");
            };
            assert_eq!(projection.binding.session_id, session_id);
            assert_eq!(projection.binding.session_generation, 4);
            assert!(projection.attempt_id.is_none());
            assert!(
                controller
                    .begin_attempt(&pane_id, &tab_id, projection.binding, target.clone())
                    .is_some()
            );
        }

        for lifecycle in [
            SessionLifecycle::Starting,
            SessionLifecycle::Closing,
            SessionLifecycle::Closed,
        ] {
            let (tab_id, pane_id, session_id) = identity();
            let mut controller = SessionReconnectController::default();
            observe(
                &mut controller,
                &tab_id,
                &pane_id,
                &session_id,
                target.clone(),
                lifecycle,
                5,
            );
            assert!(
                controller
                    .action_projection(
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
}

/// 所有真实 transport 的取消或失败收到服务回执后，仍能对原 session 再次尝试。
///
/// @author kongweiguang
#[test]
fn terminal_reconnect_remains_retryable_after_cancel_and_failure() {
    for target in manual_reconnect_targets() {
        let (tab_id, pane_id, session_id) = identity();
        let mut controller = SessionReconnectController::default();
        observe(
            &mut controller,
            &tab_id,
            &pane_id,
            &session_id,
            target.clone(),
            SessionLifecycle::Exited,
            7,
        );
        let binding = controller
            .action_projection(
                &pane_id,
                &tab_id,
                &session_id,
                &target,
                RuntimePanePhase::Ready,
            )
            .map(|projection| projection.binding)
            .expect("断开的终端 pane 应显示重连入口");

        let Some((cancelled_attempt, cancellation, _)) =
            controller.begin_attempt(&pane_id, &tab_id, binding.clone(), target.clone())
        else {
            panic!("首次终端重连应能发起");
        };
        assert!(controller.cancel_attempt(&pane_id, &binding, cancelled_attempt));
        assert!(
            cancellation
                .requested
                .load(std::sync::atomic::Ordering::Acquire)
        );
        let late_installed_token = tokio_util::sync::CancellationToken::new();
        cancellation.install(late_installed_token.clone());
        assert!(late_installed_token.is_cancelled());
        assert!(matches!(
            controller.action_projection(
                &pane_id,
                &tab_id,
                &session_id,
                &target,
                RuntimePanePhase::Ready,
            ),
            Some(projection)
                if projection.busy
                    && projection.cancelling
                    && projection.attempt_id == Some(cancelled_attempt)
        ));
        assert!(
            controller
                .begin_attempt(&pane_id, &tab_id, binding.clone(), target.clone())
                .is_none(),
            "取消回执到达前必须保留唯一 attempt owner"
        );
        assert!(
            controller
                .finish_attempt(
                    cancelled_attempt,
                    Err(kerminal_application::AppError::cancelled("测试取消")),
                )
                .is_some()
        );
        assert!(matches!(
            controller.action_projection(
                &pane_id,
                &tab_id,
                &session_id,
                &target,
                RuntimePanePhase::Ready,
            ),
            Some(projection)
                if !projection.busy
                    && projection.attempt_id.is_none()
                    && controller
                        .panes
                        .get(&pane_id)
                        .is_some_and(|state| state.result == ReconnectResultState::Cancelled)
        ));

        let Some((failed_attempt, _, _)) =
            controller.begin_attempt(&pane_id, &tab_id, binding.clone(), target.clone())
        else {
            panic!("取消 ACK 后应允许再次重连");
        };
        assert!(
            controller
                .finish_attempt(
                    failed_attempt,
                    Err(kerminal_application::AppError::infrastructure(
                        "测试连接失败"
                    )),
                )
                .is_some()
        );
        let failed_projection = controller.action_projection(
            &pane_id,
            &tab_id,
            &session_id,
            &target,
            RuntimePanePhase::Ready,
        );
        assert!(matches!(
            failed_projection,
            Some(projection)
                if projection.attempt_id.is_none()
                    && controller
                        .panes
                        .get(&pane_id)
                        .is_some_and(|state| {
                            matches!(state.result, ReconnectResultState::Failed(_))
                        })
        ));
        assert!(
            controller
                .begin_attempt(&pane_id, &tab_id, binding, target)
                .is_some()
        );
    }
}

#[path = "root_protocol_reconnect_identity.rs"]
mod identity_tests;
