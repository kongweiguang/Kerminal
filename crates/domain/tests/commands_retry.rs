//! 工作流失败重试只接受明确的非零执行结果。
//!
//! @author kongweiguang

use kerminal_domain::commands::{
    CommandWorkflow, CommandWorkflowStep, WorkflowRun, WorkflowRunState, WorkflowScope,
    WorkflowStepState,
};

/// 显式重试会复用失败步骤并保留之前步骤记录，不会重跑整条工作流。
#[test]
fn retry_resets_only_the_known_failed_step() {
    let workflow = CommandWorkflow {
        id: "deploy".to_owned(),
        title: "部署".to_owned(),
        description: None,
        tags: Vec::new(),
        scope: WorkflowScope::Local,
        steps: vec![step("prepare", 0), step("deploy", 1)],
        sort_order: 0,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    };
    let mut run = WorkflowRun::start("run-1", &workflow).expect("workflow is non-empty");
    run.begin_current_step().expect("first step starts");
    run.complete_current_step(0).expect("first step succeeds");
    run.begin_current_step().expect("second step starts");
    run.complete_current_step(2).expect("second step fails");

    run.retry_failed_current_step()
        .expect("explicit retry starts the failed step");

    assert_eq!(run.state(), WorkflowRunState::Running);
    assert_eq!(run.current_step(), 1);
    assert_eq!(run.results()[0].state, WorkflowStepState::Succeeded);
    assert_eq!(run.results()[1].state, WorkflowStepState::Running);
    assert_eq!(run.results()[1].exit_code, None);
}

/// 工作流没有已知非零退出状态时禁止重试，避免把未知结果当作安全失败。
#[test]
fn retry_rejects_non_failed_workflow() {
    let workflow = CommandWorkflow {
        id: "deploy".to_owned(),
        title: "部署".to_owned(),
        description: None,
        tags: Vec::new(),
        scope: WorkflowScope::Local,
        steps: vec![step("prepare", 0)],
        sort_order: 0,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    };
    let mut run = WorkflowRun::start("run-1", &workflow).expect("workflow is non-empty");

    assert!(run.retry_failed_current_step().is_err());
}

/// 运行中取消保留未知结果，用户不能把未知副作用步骤当作已知失败直接重试。
#[test]
fn cancellation_marks_running_step_unknown_and_not_retryable() {
    let workflow = CommandWorkflow {
        id: "deploy".to_owned(),
        title: "部署".to_owned(),
        description: None,
        tags: Vec::new(),
        scope: WorkflowScope::Local,
        steps: vec![step("deploy", 0)],
        sort_order: 0,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    };
    let mut run = WorkflowRun::start("run-1", &workflow).expect("workflow is non-empty");
    run.begin_current_step().expect("step starts");

    run.cancel().expect("running workflow can be cancelled");

    assert_eq!(run.state(), WorkflowRunState::Cancelled);
    assert_eq!(run.results()[0].state, WorkflowStepState::Unknown);
    assert!(run.retry_failed_current_step().is_err());
}

/// 缺少终端完成证据时产生未知结果，只有带非零退出码的失败才进入显式重试路径。
#[test]
fn unknown_result_cannot_be_retried() {
    let workflow = CommandWorkflow {
        id: "deploy".to_owned(),
        title: "部署".to_owned(),
        description: None,
        tags: Vec::new(),
        scope: WorkflowScope::Local,
        steps: vec![step("deploy", 0)],
        sort_order: 0,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    };
    let mut run = WorkflowRun::start("run-1", &workflow).expect("workflow is non-empty");
    run.begin_current_step().expect("step starts");
    run.mark_current_step_unknown()
        .expect("missing event is unknown");

    assert_eq!(run.state(), WorkflowRunState::Failed);
    assert_eq!(run.results()[0].state, WorkflowStepState::Unknown);
    assert!(run.retry_failed_current_step().is_err());
}

/// 写入前被拒绝的步骤允许显式重试，但不会伪造退出码或把未知结果归入该路径。
#[test]
fn explicit_retry_accepts_only_a_known_not_started_step() {
    let workflow = CommandWorkflow {
        id: "deploy".to_owned(),
        title: "部署".to_owned(),
        description: None,
        tags: Vec::new(),
        scope: WorkflowScope::Local,
        steps: vec![step("deploy", 0)],
        sort_order: 0,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    };
    let mut run = WorkflowRun::start("run-1", &workflow).expect("workflow is non-empty");
    run.begin_current_step().expect("step starts");
    run.mark_current_step_not_started()
        .expect("terminal refused before input");

    run.retry_failed_current_step()
        .expect("explicit retry is safe");

    assert_eq!(run.state(), WorkflowRunState::Running);
    assert_eq!(run.results()[0].state, WorkflowStepState::Running);
    assert_eq!(run.results()[0].exit_code, None);
}

/// 为领域状态测试构造最小、显式排序的工作流步骤。
fn step(id: &str, sort_order: i64) -> CommandWorkflowStep {
    CommandWorkflowStep {
        id: id.to_owned(),
        title: id.to_owned(),
        command: format!("echo {id}"),
        description: None,
        scope: None,
        requires_confirmation: false,
        sort_order,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    }
}
