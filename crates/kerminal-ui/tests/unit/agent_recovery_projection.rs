//! Agent 恢复入口的纯投影回归，不创建窗口、CLI 或真实工作区。
//!
//! @author kongweiguang

use kerminal_application::agents::AgentResumeCapability;
use kerminal_application::{PublicErrorCode, agents::AgentResumeRecovery};
use kerminal_domain::{
    agents::{AgentProvider, AgentSession, AgentSessionLaunch},
    identity::SessionId,
    sessions::{
        LocalSessionTarget, SessionExitStatus, SessionInfo, SessionOwner, SessionTarget,
        TerminalSize,
    },
};

use super::*;

/// 同一 Agent 与 terminal 身份用于切换生命周期，避免测试用替换 ID 绕过恢复门禁。
///
/// @author kongweiguang
fn recovery_view(lifecycle: SessionLifecycle) -> AgentSessionView {
    let id = AgentSessionId::new();
    let session = AgentSession::new(
        id.clone(),
        AgentProvider::Codex,
        None,
        "Recovery fixture".to_owned(),
        "2026-10-19T00:00:00Z".to_owned(),
        "workspace".to_owned(),
        "workspace/agents/session".to_owned(),
        AgentSessionLaunch {
            command_label: "Codex".to_owned(),
            shell: "codex".to_owned(),
            args: Vec::new(),
            cwd: "workspace/agents/session".to_owned(),
        },
        None,
        None,
    )
    .expect("valid isolated Agent fixture");
    AgentSessionView {
        session,
        resume_capability: AgentResumeCapability::MetadataUnavailable,
        terminal: Some(SessionInfo {
            session_id: SessionId::new(),
            target: SessionTarget::Local(LocalSessionTarget {
                program: "codex".into(),
                args: Vec::new(),
                working_directory: None,
                environment: Default::default(),
            }),
            owner: SessionOwner::Agent {
                agent_session_id: id,
            },
            size: TerminalSize::default(),
            lifecycle,
            exit_status: None,
        }),
        resume_recovery: None,
    }
}

/// 以真实保存的启动标签构造 Custom 快照，验证展示只依赖已有投影而不查询当前设置。
///
/// @author kongweiguang
fn custom_session_view(title: &str, launcher_label: &str) -> AgentSessionView {
    let session = AgentSession::new(
        AgentSessionId::new(),
        AgentProvider::Custom,
        None,
        title.to_owned(),
        "2026-10-19T00:00:00Z".to_owned(),
        "workspace".to_owned(),
        "workspace/agents/session".to_owned(),
        AgentSessionLaunch {
            command_label: launcher_label.to_owned(),
            shell: "echo".to_owned(),
            args: Vec::new(),
            cwd: "workspace/agents/session".to_owned(),
        },
        None,
        None,
    )
    .expect("valid isolated Custom Agent fixture");
    AgentSessionView {
        session,
        resume_capability: AgentResumeCapability::Unsupported,
        terminal: None,
        resume_recovery: None,
    }
}

/// Custom 的通用默认标题改用快照中的 launcher 名，显式会话标题和内建 provider 标题仍原样展示。
///
/// @author kongweiguang
#[test]
fn picker_uses_saved_custom_launcher_name_without_overriding_explicit_title() {
    let launcher_name = "本地 Echo 验收（无外部调用）";
    let mut custom = custom_session_view("Custom Agent", launcher_name);
    assert_eq!(session_picker_label(&custom, true), launcher_name);

    custom.session.title = "手动会话标题".to_owned();
    assert_eq!(session_picker_label(&custom, true), "手动会话标题");

    let mut builtin = recovery_view(SessionLifecycle::Exited);
    builtin.session.title = "Custom Agent".to_owned();
    assert_eq!(session_picker_label(&builtin, true), "Custom Agent");
}

/// 从运行或已退出的会话进入新建态只发出空选择事件；原列表快照和 terminal owner 仍完整。
///
/// @author kongweiguang
#[test]
fn opening_new_chat_returns_to_launcher_without_closing_running_or_exited_owner() {
    for lifecycle in [SessionLifecycle::Running, SessionLifecycle::Exited] {
        let view = recovery_view(lifecycle);
        let original_session_id = view
            .terminal
            .as_ref()
            .expect("fixture keeps its terminal owner")
            .session_id
            .clone();

        assert!(has_selected_agent_terminal_surface(Some(&view)));
        assert!(!has_selected_agent_terminal_surface(None));
        assert_eq!(
            super::super::actions::new_chat_selection_event(),
            crate::features::agents::AgentPanelEvent::Selected {
                agent_session_id: None,
                terminal_session_id: None,
            }
        );

        let terminal = view.terminal.as_ref().expect("owner remains in the view");
        assert_eq!(terminal.session_id, original_session_id);
        assert_eq!(terminal.lifecycle, lifecycle);
    }
}

/// Pending 和 Unknown 无论 owner 运行或退出，都只能核对而不能继续启动另一个 CLI。
///
/// @author kongweiguang
#[test]
fn unresolved_recovery_exposes_reconciliation_instead_of_resume() {
    for lifecycle in [SessionLifecycle::Running, SessionLifecycle::Exited] {
        for status in [
            AgentResumeRecoveryStatus::Pending,
            AgentResumeRecoveryStatus::Unknown,
        ] {
            let mut view = recovery_view(lifecycle);
            view.resume_recovery = Some(AgentResumeRecovery {
                terminal_session_id: view
                    .terminal
                    .as_ref()
                    .expect("fixture owner")
                    .session_id
                    .clone(),
                status,
            });
            assert!(agent_can_reconcile(&view));
            assert!(!agent_can_resume(&view));
            assert!(!has_agent_terminal_surface(&view));
        }
    }
}
/// 未决恢复禁止归档，但不隐藏核对入口；正常未归档记录仍可归档。
///
/// @author kongweiguang
#[test]
fn unresolved_recovery_disables_archive_and_keeps_reconciliation_available() {
    for status in [
        AgentResumeRecoveryStatus::Pending,
        AgentResumeRecoveryStatus::Unknown,
    ] {
        let mut view = recovery_view(SessionLifecycle::Exited);
        view.resume_recovery = Some(AgentResumeRecovery {
            terminal_session_id: view
                .terminal
                .as_ref()
                .expect("fixture owner")
                .session_id
                .clone(),
            status,
        });
        assert!(!agent_can_archive(&view));
        assert!(agent_can_reconcile(&view));
    }

    let active = recovery_view(SessionLifecycle::Running);
    assert!(agent_can_archive(&active));
    let mut archived = recovery_view(SessionLifecycle::Exited);
    archived.session.status = AgentSessionStatus::Archived;
    assert!(!agent_can_archive(&archived));
}

/// 核对结束后只有 terminal 终态或缺失才开放继续；关闭中和启动中仍保留 owner。
///
/// @author kongweiguang
#[test]
fn confirmed_projection_resumes_only_after_terminal_owner_has_stopped() {
    for lifecycle in [
        SessionLifecycle::Starting,
        SessionLifecycle::Running,
        SessionLifecycle::Closing,
    ] {
        let mut view = recovery_view(lifecycle);
        view.resume_capability = AgentResumeCapability::Supported;
        assert!(!agent_can_resume(&view));
        assert!(!agent_can_reconcile(&view));
    }
    for lifecycle in [
        SessionLifecycle::Exited,
        SessionLifecycle::Closed,
        SessionLifecycle::Failed,
    ] {
        let mut view = recovery_view(lifecycle);
        view.resume_capability = AgentResumeCapability::Supported;
        assert!(agent_can_resume(&view));
    }
    let mut missing = recovery_view(SessionLifecycle::Exited);
    missing.resume_capability = AgentResumeCapability::Supported;
    missing.terminal = None;
    assert!(agent_can_resume(&missing));
    let mut archived = recovery_view(SessionLifecycle::Exited);
    archived.session.status = AgentSessionStatus::Archived;
    archived.resume_recovery = Some(AgentResumeRecovery {
        terminal_session_id: SessionId::new(),
        status: AgentResumeRecoveryStatus::Unknown,
    });
    assert!(!agent_can_resume(&archived));
    assert!(!agent_can_reconcile(&archived));
}

/// 活终端仅省略重复成功提示，失败、取消中和未知结果必须保留面板反馈。
///
/// @author kongweiguang
#[test]
fn terminal_projection_does_not_hide_failure_or_cancellation_feedback() {
    assert!(is_terminal_success_notice("Agent CLI 已启动。"));
    assert!(!is_terminal_success_notice("已请求取消，正在等待实际结果…"));
    assert!(!is_terminal_success_notice("Agent 操作失败"));
    assert!(!is_terminal_success_notice("恢复结果未知"));
}

/// 仅确定失败给出配置下一步；取消与未知回执禁止诱导用户改设置后重放。
///
/// @author kongweiguang
#[test]
fn launcher_setup_help_preserves_cancel_and_unknown_outcome_boundaries() {
    use kerminal_application::PublicErrorCode;

    assert!(super::super::actions::should_offer_launcher_setup_help(
        PublicErrorCode::Infrastructure
    ));
    assert!(!super::super::actions::should_offer_launcher_setup_help(
        PublicErrorCode::Cancelled
    ));
    assert!(!super::super::actions::should_offer_launcher_setup_help(
        PublicErrorCode::OutcomeUnknown
    ));
    assert!(!super::super::actions::should_offer_launcher_setup_help(
        PublicErrorCode::AuthenticationFailed
    ));
}

/// 无选中会话不显示空操作菜单；恢复待核对的有效会话仍保留核对入口。
///
/// @author kongweiguang
#[test]
fn actions_menu_requires_a_valid_selection_but_keeps_unresolved_recovery_accessible() {
    assert!(!should_render_actions_menu(None));
    for status in [
        AgentResumeRecoveryStatus::Pending,
        AgentResumeRecoveryStatus::Unknown,
    ] {
        let mut view = recovery_view(SessionLifecycle::Exited);
        view.resume_recovery = Some(AgentResumeRecovery {
            terminal_session_id: view
                .terminal
                .as_ref()
                .expect("fixture keeps its original terminal owner")
                .session_id
                .clone(),
            status,
        });
        assert!(should_render_actions_menu(Some(&view)));
        assert!(agent_can_reconcile(&view));
    }
}

/// 正常状态要求明确零码成功；非零/信号保留失败，缺少或矛盾回执不冒称成功。
///
/// @author kongweiguang
#[test]
fn exited_agent_projection_distinguishes_success_failure_and_unconfirmed_result() {
    let mut normal = recovery_view(SessionLifecycle::Exited);
    normal
        .terminal
        .as_mut()
        .expect("fixture terminal")
        .exit_status = Some(SessionExitStatus {
        code: Some(0),
        signal: None,
        success: true,
    });
    assert_eq!(
        terminal_exit_projection(&normal),
        Some(TerminalExitProjection::Normal)
    );
    assert_eq!(
        lifecycle_label(&normal),
        crate::locale::text("已正常退出", "Exited normally").to_string()
    );
    assert_eq!(
        terminal_header_status(Some(&normal), None)
            .expect("zero exit has a neutral header state")
            .tone,
        TerminalStatusTone::Neutral
    );

    let mut nonzero = recovery_view(SessionLifecycle::Exited);
    nonzero
        .terminal
        .as_mut()
        .expect("fixture terminal")
        .exit_status = Some(SessionExitStatus {
        code: Some(7),
        signal: None,
        success: false,
    });
    assert_eq!(
        terminal_exit_projection(&nonzero),
        Some(TerminalExitProjection::Failure)
    );
    assert!(
        terminal_exit_message(&nonzero)
            .expect("failed exit has a visible summary")
            .0
            .contains("7")
    );
    assert_eq!(
        terminal_header_status(Some(&nonzero), None)
            .expect("nonzero exit remains visible")
            .tone,
        TerminalStatusTone::Error
    );

    let mut signaled = recovery_view(SessionLifecycle::Exited);
    signaled
        .terminal
        .as_mut()
        .expect("fixture terminal")
        .exit_status = Some(SessionExitStatus {
        code: None,
        signal: Some("SIGTERM".to_owned()),
        success: false,
    });
    assert_eq!(
        terminal_exit_projection(&signaled),
        Some(TerminalExitProjection::Failure)
    );

    let missing = recovery_view(SessionLifecycle::Exited);
    assert_eq!(
        terminal_exit_projection(&missing),
        Some(TerminalExitProjection::Unknown)
    );
    assert_eq!(
        terminal_header_status(Some(&missing), None)
            .expect("missing status is not reported as success")
            .tone,
        TerminalStatusTone::Warning
    );
    let mut contradictory = recovery_view(SessionLifecycle::Exited);
    contradictory
        .terminal
        .as_mut()
        .expect("fixture terminal")
        .exit_status = Some(SessionExitStatus {
        code: Some(0),
        signal: None,
        success: false,
    });
    assert_eq!(
        terminal_exit_projection(&contradictory),
        Some(TerminalExitProjection::Failure)
    );

    let runtime_feedback = AgentTerminalFeedback {
        agent_session_id: normal.session.agent_session_id.clone(),
        message: "snapshot read failed".to_owned(),
        source: AgentTerminalFeedbackSource::RuntimeFailure,
    };
    let header = terminal_header_status(Some(&normal), Some(&runtime_feedback))
        .expect("real runtime failures are not hidden by a successful exit code");
    assert_eq!(header.message, "snapshot read failed");
    assert_eq!(header.tone, TerminalStatusTone::Error);
}

/// Continue 顶栏入口只投影现有恢复门禁，Pending/Unknown 不会被退出态 CTA 绕过。
///
/// @author kongweiguang
#[test]
fn visible_continue_uses_existing_resume_gate() {
    assert!(!should_render_continue(None));
    let mut running = recovery_view(SessionLifecycle::Running);
    running.resume_capability = AgentResumeCapability::Supported;
    assert!(!should_render_continue(Some(&running)));
    let mut exited = recovery_view(SessionLifecycle::Exited);
    exited.resume_capability = AgentResumeCapability::Supported;
    assert!(should_render_continue(Some(&exited)));

    let mut unresolved = recovery_view(SessionLifecycle::Exited);
    unresolved.resume_capability = AgentResumeCapability::Supported;
    unresolved.resume_recovery = Some(AgentResumeRecovery {
        terminal_session_id: unresolved
            .terminal
            .as_ref()
            .expect("fixture terminal")
            .session_id
            .clone(),
        status: AgentResumeRecoveryStatus::Unknown,
    });
    assert!(!should_render_continue(Some(&unresolved)));
}

/// 独立状态行保留退出/运行信息时，picker 不再把同一状态挤进会话标题。
///
/// @author kongweiguang
#[test]
fn session_picker_separates_title_from_visible_terminal_status() {
    let running = recovery_view(SessionLifecycle::Running);
    assert_eq!(session_picker_label(&running, true), running.session.title);

    let inline = session_picker_label(&running, false);
    assert!(inline.starts_with(&running.session.title));
    assert!(inline.ends_with(&lifecycle_label(&running)));

    let status = terminal_header_status_row(Some(&running), true, None, false)
        .expect("active terminal keeps a visible status row");
    assert_eq!(status.message, lifecycle_label(&running));
}

/// 只有已核实 provider 恢复能力才暴露 Continue；未知或明确不支持仍保留历史新建入口。
///
/// @author kongweiguang
#[test]
fn resume_requires_verified_provider_capability() {
    let mut view = recovery_view(SessionLifecycle::Exited);
    for capability in [
        AgentResumeCapability::Unsupported,
        AgentResumeCapability::MetadataUnavailable,
    ] {
        view.resume_capability = capability;
        assert!(!agent_can_resume(&view));
        assert!(!should_render_continue(Some(&view)));
        assert!(view.resume_recovery.is_none());
    }
    view.resume_capability = AgentResumeCapability::Supported;
    assert!(agent_can_resume(&view));
    assert!(should_render_continue(Some(&view)));
}

/// 只有同一 Agent 的 Conflict/NotFound 操作拒绝可由 Exited 读回收敛；真实错误和未知回执保留。
///
/// @author kongweiguang
#[test]
fn terminal_operation_feedback_clears_only_after_same_agent_exited_readback() {
    let exited = recovery_view(SessionLifecycle::Exited);
    let id = exited.session.agent_session_id.clone();
    for code in [PublicErrorCode::Conflict, PublicErrorCode::NotFound] {
        let feedback = AgentTerminalFeedback {
            agent_session_id: id.clone(),
            message: "typed terminal operation rejection".to_owned(),
            source: AgentTerminalFeedbackSource::OperationFailure(code),
        };
        assert!(
            super::super::actions::should_clear_terminal_feedback_after_reload(
                &feedback,
                Some(&id),
                std::slice::from_ref(&exited),
            )
        );
        assert!(
            !super::super::actions::should_clear_terminal_feedback_after_reload(
                &feedback,
                Some(&AgentSessionId::new()),
                std::slice::from_ref(&exited),
            )
        );
        assert!(
            !super::super::actions::should_clear_terminal_feedback_after_reload(
                &feedback,
                Some(&id),
                &[],
            )
        );
    }

    for source in [
        AgentTerminalFeedbackSource::RuntimeFailure,
        AgentTerminalFeedbackSource::OperationFailure(PublicErrorCode::OutcomeUnknown),
        AgentTerminalFeedbackSource::OperationFailure(PublicErrorCode::Infrastructure),
    ] {
        let feedback = AgentTerminalFeedback {
            agent_session_id: id.clone(),
            message: "persistent terminal failure".to_owned(),
            source,
        };
        assert!(
            !super::super::actions::should_clear_terminal_feedback_after_reload(
                &feedback,
                Some(&id),
                std::slice::from_ref(&exited),
            )
        );
    }

    let feedback = AgentTerminalFeedback {
        agent_session_id: id.clone(),
        message: "wait for exit".to_owned(),
        source: AgentTerminalFeedbackSource::OperationFailure(PublicErrorCode::Conflict),
    };
    let running = recovery_view(SessionLifecycle::Running);
    assert!(
        !super::super::actions::should_clear_terminal_feedback_after_reload(
            &feedback,
            Some(&id),
            std::slice::from_ref(&running),
        )
    );
}

/// 较窄的退出拒绝不覆盖真实 runtime 故障或尚未核实的操作结果。
///
/// @author kongweiguang
#[test]
fn terminal_feedback_precedence_preserves_runtime_and_unknown_results() {
    let id = AgentSessionId::new();
    let feedback = |source| AgentTerminalFeedback {
        agent_session_id: id.clone(),
        message: "fixture".to_owned(),
        source,
    };
    for source in [
        AgentTerminalFeedbackSource::RuntimeFailure,
        AgentTerminalFeedbackSource::OperationFailure(PublicErrorCode::OutcomeUnknown),
        AgentTerminalFeedbackSource::OperationFailure(PublicErrorCode::Infrastructure),
    ] {
        let persistent = feedback(source);
        let narrow = feedback(AgentTerminalFeedbackSource::OperationFailure(
            PublicErrorCode::Conflict,
        ));
        assert!(!super::super::terminal_feedback_should_replace(
            &persistent,
            &narrow
        ));
    }
    let narrow = feedback(AgentTerminalFeedbackSource::OperationFailure(
        PublicErrorCode::Conflict,
    ));
    let unknown = feedback(AgentTerminalFeedbackSource::OperationFailure(
        PublicErrorCode::OutcomeUnknown,
    ));
    assert!(super::super::terminal_feedback_should_replace(
        &narrow, &unknown
    ));
}

/// 旧 PTY 故障只在显式 Resume 成功返回同 Agent 的新 Running owner 后收敛。
///
/// @author kongweiguang
#[test]
fn old_terminal_feedback_clears_only_for_a_new_running_resume_owner() {
    let exited = recovery_view(SessionLifecycle::Exited);
    let agent_id = exited.session.agent_session_id.clone();
    let previous_terminal_id = exited
        .terminal
        .as_ref()
        .expect("fixture includes the old terminal")
        .session_id
        .clone();
    let reset = super::super::AgentTerminalFeedbackReset::OnNewRunningTerminal {
        previous_terminal_session_id: Some(previous_terminal_id.clone()),
    };

    let mut resumed = recovery_view(SessionLifecycle::Running);
    resumed.session.agent_session_id = agent_id.clone();
    resumed.terminal.as_mut().expect("new terminal").owner = SessionOwner::Agent {
        agent_session_id: agent_id.clone(),
    };
    assert_ne!(
        resumed.terminal.as_ref().expect("new terminal").session_id,
        previous_terminal_id
    );
    assert!(
        super::super::actions::should_clear_terminal_feedback_after_lifecycle_success(
            &reset, &agent_id, &resumed
        )
    );

    let mut same_terminal = recovery_view(SessionLifecycle::Running);
    same_terminal.session.agent_session_id = agent_id.clone();
    same_terminal
        .terminal
        .as_mut()
        .expect("same terminal fixture")
        .session_id = previous_terminal_id;
    same_terminal
        .terminal
        .as_mut()
        .expect("same terminal owner")
        .owner = SessionOwner::Agent {
        agent_session_id: agent_id.clone(),
    };
    assert!(
        !super::super::actions::should_clear_terminal_feedback_after_lifecycle_success(
            &reset,
            &agent_id,
            &same_terminal
        )
    );

    assert!(
        !super::super::actions::should_clear_terminal_feedback_after_lifecycle_success(
            &super::super::AgentTerminalFeedbackReset::Never,
            &agent_id,
            &resumed
        )
    );
    assert!(
        !super::super::actions::should_clear_terminal_feedback_after_lifecycle_success(
            &reset,
            &AgentSessionId::new(),
            &resumed
        )
    );

    let mut wrong_owner = resumed.clone();
    wrong_owner
        .terminal
        .as_mut()
        .expect("terminal fixture")
        .owner = SessionOwner::Agent {
        agent_session_id: AgentSessionId::new(),
    };
    assert!(
        !super::super::actions::should_clear_terminal_feedback_after_lifecycle_success(
            &reset,
            &agent_id,
            &wrong_owner
        )
    );

    assert!(
        !super::super::actions::should_clear_terminal_feedback_after_lifecycle_success(
            &reset, &agent_id, &exited
        )
    );
    let mut no_terminal = resumed.clone();
    no_terminal.terminal = None;
    assert!(
        !super::super::actions::should_clear_terminal_feedback_after_lifecycle_success(
            &reset,
            &agent_id,
            &no_terminal
        )
    );

    let reset_without_previous = super::super::AgentTerminalFeedbackReset::OnNewRunningTerminal {
        previous_terminal_session_id: None,
    };
    for status in [
        AgentResumeRecoveryStatus::Pending,
        AgentResumeRecoveryStatus::Unknown,
    ] {
        let mut unconfirmed = resumed.clone();
        unconfirmed.resume_recovery = Some(AgentResumeRecovery {
            terminal_session_id: unconfirmed
                .terminal
                .as_ref()
                .expect("new terminal")
                .session_id
                .clone(),
            status,
        });
        assert!(
            !super::super::actions::should_clear_terminal_feedback_after_lifecycle_success(
                &reset,
                &agent_id,
                &unconfirmed
            )
        );
    }
    let mut stale = resumed.clone();
    stale.session.status = AgentSessionStatus::Stale;
    assert!(
        !super::super::actions::should_clear_terminal_feedback_after_lifecycle_success(
            &reset, &agent_id, &stale
        )
    );
    assert!(
        super::super::actions::should_clear_terminal_feedback_after_lifecycle_success(
            &reset_without_previous,
            &agent_id,
            &resumed
        )
    );
}
