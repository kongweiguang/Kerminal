//! 终端重连的身份绑定、generation 与 pane 关闭竞争契约。
//!
//! @author kongweiguang

use super::super::{
    ReconnectObservation, SessionReconnectController, SessionReconnectState,
    validate_reconnect_after, validate_reconnect_before,
};
use super::{identity, manual_reconnect_targets, observation, observe};
use crate::root::workspace_runtime::RuntimePanePhase;
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    identity::{SessionId, TabId},
    sessions::{SessionInfo, SessionLifecycle, SessionOwner, SessionTarget, TelnetSessionTarget},
};

/// 每种 transport 的请求前后校验都绑定精确 session、UI owner、target 和 generation。
///
/// @author kongweiguang
#[test]
fn reconnect_validation_binds_every_transport_identity_and_generation() {
    for target in manual_reconnect_targets() {
        let (tab_id, pane_id, session_id) = identity();
        let owner = SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        };
        let binding = UiSessionBinding {
            session_id: session_id.clone(),
            owner: owner.clone(),
            session_generation: 8,
        };
        let before = observation(
            &tab_id,
            &pane_id,
            &session_id,
            target.clone(),
            SessionLifecycle::Exited,
            8,
        );

        assert!(validate_reconnect_before(&before, &binding, &target, &tab_id, &pane_id).is_ok());

        let stale_generation = UiSessionBinding {
            session_generation: 7,
            ..binding.clone()
        };
        assert!(
            validate_reconnect_before(&before, &stale_generation, &target, &tab_id, &pane_id,)
                .is_err()
        );

        let wrong_owner = SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: pane_id.clone(),
        };
        let wrong_binding = UiSessionBinding {
            owner: wrong_owner,
            ..binding.clone()
        };
        assert!(
            validate_reconnect_before(&before, &wrong_binding, &target, &tab_id, &pane_id,)
                .is_err()
        );

        let replacement_target = SessionTarget::Telnet(TelnetSessionTarget {
            host: "other.example".into(),
            port: 23,
        });
        assert!(
            validate_reconnect_before(&before, &binding, &replacement_target, &tab_id, &pane_id,)
                .is_err()
        );

        let after = observation(
            &tab_id,
            &pane_id,
            &session_id,
            target.clone(),
            SessionLifecycle::Running,
            9,
        );
        assert!(validate_reconnect_after(&after, &before, &binding, &target).is_ok());

        let same_generation = observation(
            &tab_id,
            &pane_id,
            &session_id,
            target.clone(),
            SessionLifecycle::Running,
            8,
        );
        assert!(validate_reconnect_after(&same_generation, &before, &binding, &target).is_err());

        let changed_target = observation(
            &tab_id,
            &pane_id,
            &session_id,
            replacement_target,
            SessionLifecycle::Running,
            9,
        );
        assert!(validate_reconnect_after(&changed_target, &before, &binding, &target).is_err());

        let changed_owner = ReconnectObservation {
            info: SessionInfo {
                owner: SessionOwner::Ui {
                    tab_id: TabId::new(),
                    pane_id: pane_id.clone(),
                },
                ..after.info.clone()
            },
            session_generation: after.session_generation,
        };
        assert!(validate_reconnect_after(&changed_owner, &before, &binding, &target).is_err());

        let changed_session = observation(
            &tab_id,
            &pane_id,
            &SessionId::new(),
            target.clone(),
            SessionLifecycle::Running,
            9,
        );
        assert!(validate_reconnect_after(&changed_session, &before, &binding, &target).is_err());
    }
}

/// pane 关闭或替换时取消旧 attempt；其迟到回执不能接管新 pane 的同 ID 终端。
///
/// @author kongweiguang
#[test]
fn late_transport_ack_cannot_replace_any_new_pane_incarnation() {
    for target in manual_reconnect_targets() {
        let (old_tab, pane_id, old_session) = identity();
        let mut controller = SessionReconnectController::default();
        observe(
            &mut controller,
            &old_tab,
            &pane_id,
            &old_session,
            target.clone(),
            SessionLifecycle::Failed,
            12,
        );
        let old_binding = controller
            .projection(
                &pane_id,
                &old_tab,
                &old_session,
                &target,
                RuntimePanePhase::Ready,
            )
            .expect("故障终端应生成原位重连入口")
            .binding;
        let (old_attempt, cancellation) = controller
            .begin_attempt(&pane_id, &old_tab, old_binding.clone(), target.clone())
            .expect("首次手动重连应发起");

        controller.cancel_for_pane(&pane_id);
        assert!(
            cancellation
                .requested
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert_eq!(
            controller.in_flight.len(),
            1,
            "pane 取消后 owner 保留到 ACK"
        );

        let (new_tab, _, new_session) = identity();
        observe(
            &mut controller,
            &new_tab,
            &pane_id,
            &new_session,
            target.clone(),
            SessionLifecycle::Exited,
            2,
        );
        assert!(
            controller
                .projection(
                    &pane_id,
                    &new_tab,
                    &new_session,
                    &target,
                    RuntimePanePhase::Ready,
                )
                .is_none()
        );
        let new_binding = UiSessionBinding {
            session_id: new_session.clone(),
            owner: SessionOwner::Ui {
                tab_id: new_tab.clone(),
                pane_id: pane_id.clone(),
            },
            session_generation: 2,
        };
        assert!(
            controller
                .begin_attempt(&pane_id, &new_tab, new_binding.clone(), target.clone())
                .is_none(),
            "旧 transport 未确认关闭前，不得为新 pane 启动重连"
        );

        let late_old_ack = observation(
            &old_tab,
            &pane_id,
            &old_session,
            target.clone(),
            SessionLifecycle::Running,
            13,
        );
        assert!(!controller.finish_attempt(old_attempt, Ok(late_old_ack)));
        assert!(matches!(
            controller.projection(
                &pane_id,
                &new_tab,
                &new_session,
                &target,
                RuntimePanePhase::Ready,
            ),
            Some(projection)
                if projection.binding == new_binding
                    && projection.state == SessionReconnectState::Available
        ));
        assert!(
            controller
                .begin_attempt(&pane_id, &new_tab, new_binding, target)
                .is_some(),
            "旧 attempt ACK 后新 pane 可按自身身份重试"
        );
    }
}
