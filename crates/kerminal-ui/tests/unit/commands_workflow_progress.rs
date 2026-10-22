//! 工作流完整快照的身份门禁和取消文案回归，不依赖真实窗口或终端进程。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString};

use kerminal_application::commands::{WorkflowRunProgress, WorkflowStepProgress};
use kerminal_domain::{
    commands::{
        CommandWorkflow, CommandWorkflowStep, WorkflowRun, WorkflowRunState, WorkflowScope,
        WorkflowStepState,
    },
    identity::SessionId,
    sessions::{LocalSessionTarget, SessionTarget},
};

use super::{
    command_message, workflow_progress_display_status, workflow_progress_matches_run,
    workflow_unknown_review_snapshot,
};

/// 使用真实运行聚合与单步骤快照，确保测试在相同身份和 revision 规则上运行。
/// @author kongweiguang
fn fixture() -> (WorkflowRun, WorkflowRunProgress) {
    let workflow = CommandWorkflow {
        id: "workflow".to_owned(),
        title: "Fixture".to_owned(),
        description: None,
        tags: Vec::new(),
        scope: WorkflowScope::Local,
        steps: vec![CommandWorkflowStep {
            id: "step".to_owned(),
            title: "Step".to_owned(),
            command: "echo safe".to_owned(),
            description: None,
            scope: Some(WorkflowScope::Local),
            requires_confirmation: false,
            sort_order: 0,
            created_at: "2026-10-19T00:00:00Z".to_owned(),
            updated_at: "2026-10-19T00:00:00Z".to_owned(),
        }],
        sort_order: 0,
        created_at: "2026-10-19T00:00:00Z".to_owned(),
        updated_at: "2026-10-19T00:00:00Z".to_owned(),
    };
    let run = WorkflowRun::start("run", &workflow).unwrap();
    let progress = WorkflowRunProgress {
        run_id: run.id().to_owned(),
        revision: 2,
        workflow_id: workflow.id,
        session_id: SessionId::new(),
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("fixture-shell"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        state: WorkflowRunState::Running,
        current_step_index: Some(0),
        steps: vec![WorkflowStepProgress {
            step_id: "step".to_owned(),
            title: "Step".to_owned(),
            state: WorkflowStepState::Running,
            exit_code: None,
            message: None,
            command_id: Some("command".to_owned()),
            error_code: None,
        }],
    };
    (run, progress)
}

/// 迟到的相同或更低 revision 不能覆盖当前快照，其他运行和目标也不能借用相同面板。
/// @author kongweiguang
#[test]
fn progress_acceptance_requires_exact_identity_and_increasing_revision() {
    let (run, current) = fixture();
    let mut incoming = current.clone();
    assert!(!workflow_progress_matches_run(
        &incoming,
        &run,
        &current.session_id,
        Some(&current)
    ));
    incoming.revision = current.revision - 1;
    assert!(!workflow_progress_matches_run(
        &incoming,
        &run,
        &current.session_id,
        Some(&current)
    ));
    incoming.revision = current.revision + 1;
    assert!(workflow_progress_matches_run(
        &incoming,
        &run,
        &current.session_id,
        Some(&current)
    ));
    incoming.run_id = "foreign-run".to_owned();
    assert!(!workflow_progress_matches_run(
        &incoming,
        &run,
        &current.session_id,
        Some(&current)
    ));
    incoming.run_id = current.run_id.clone();
    incoming.workflow_id = "foreign-workflow".to_owned();
    assert!(!workflow_progress_matches_run(
        &incoming,
        &run,
        &current.session_id,
        Some(&current)
    ));
    incoming.workflow_id = current.workflow_id.clone();
    incoming.session_id = SessionId::new();
    assert!(!workflow_progress_matches_run(
        &incoming,
        &run,
        &current.session_id,
        Some(&current)
    ));
}

/// 用户请求取消后 Running 更新必须保持待回执文案，但真实成功到达后不再显示取消中。
/// @author kongweiguang
#[test]
fn cancellation_status_waits_for_receipt_without_overwriting_real_success() {
    let (_, mut progress) = fixture();
    assert_eq!(
        workflow_progress_display_status(&progress, true),
        command_message(
            "正在取消当前步骤并确认终端结果…",
            "Cancelling the current step and waiting for the terminal result…",
        )
    );
    progress.state = WorkflowRunState::Completed;
    progress.steps[0].state = WorkflowStepState::Succeeded;
    progress.steps[0].exit_code = Some(0);
    assert_eq!(
        workflow_progress_display_status(&progress, true),
        command_message("工作流已完成", "Workflow completed")
    );
    progress.state = WorkflowRunState::Cancelled;
    progress.steps[0].state = WorkflowStepState::Unknown;
    progress.steps[0].exit_code = None;
    assert_eq!(
        workflow_progress_display_status(&progress, true),
        command_message(
            "工作流已取消；当前步骤结果未知，请先核对终端",
            "Workflow cancelled; check the terminal because the current step result is unknown",
        )
    );
}

/// Unknown 必须保留原运行、步骤和目标直到人工核对；明确失败不应被额外拦截。
///
/// @author kongweiguang
#[test]
fn unknown_workflow_result_requires_review_before_new_run() {
    let (mut run, mut progress) = fixture();
    run.begin_current_step().expect("fixture step can start");
    run.mark_current_step_unknown()
        .expect("running fixture step can become unknown");
    progress.state = WorkflowRunState::Failed;
    progress.steps[0].state = WorkflowStepState::Unknown;

    let review = workflow_unknown_review_snapshot(&run, Some(&progress), None)
        .expect("unknown receipt must produce a review summary");
    assert_eq!(review.run_id, "run");
    assert_eq!(review.target, command_message("本地终端", "Local terminal"));
    assert_eq!(review.step_title, "Step");
    assert!(review.blocks_new_run());

    let mut reviewed = review.clone();
    assert!(reviewed.acknowledge());
    assert!(!reviewed.blocks_new_run());
    assert_eq!(reviewed.run_id, review.run_id);
    assert_eq!(reviewed.target, review.target);
    assert_eq!(reviewed.step_title, review.step_title);
    assert!(!reviewed.acknowledge());

    let (mut known_failure, _) = fixture();
    known_failure
        .begin_current_step()
        .expect("fixture step can start");
    known_failure
        .complete_current_step(1)
        .expect("non-zero receipt is a known failure");
    assert!(workflow_unknown_review_snapshot(&known_failure, None, None).is_none());
}
