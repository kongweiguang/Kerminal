//! CommandsService 工作流执行的服务级回归，使用既有内存端口固定目标与历史边界。
//!
//! @author kongweiguang

use std::time::Duration;

use kerminal_domain::commands::{
    CommandHistorySource, CommandWorkflow, CommandWorkflowStep, SnippetRisk, WorkflowRunState,
    WorkflowScope, WorkflowStepState,
};
use tokio_util::sync::CancellationToken;

use crate::{
    PublicErrorCode,
    commands::{
        WorkflowExecutionControl, WorkflowExecutionRequest, WorkflowProgressEvent,
        WorkflowProgressPublisher,
    },
    sessions::{ShellCommandOutcome, ShellCommandUnknownReason},
};

use super::{ExecutionMode, definition, world};

const WORKFLOW_ID: &str = "fixture-workflow";
const GATE_TIMEOUT: Duration = Duration::from_secs(2);

/// 构造最小的本地工作流定义，保留 service 真实消费的步骤顺序和确认标记。
/// @author kongweiguang
fn workflow(steps: &[(&str, bool)]) -> CommandWorkflow {
    CommandWorkflow {
        id: WORKFLOW_ID.to_owned(),
        title: "Workflow fixture".to_owned(),
        description: None,
        tags: Vec::new(),
        scope: WorkflowScope::Local,
        steps: steps
            .iter()
            .enumerate()
            .map(
                |(index, (command, requires_confirmation))| CommandWorkflowStep {
                    id: format!("step-{}", index + 1),
                    title: format!("Step {}", index + 1),
                    command: (*command).to_owned(),
                    description: None,
                    scope: Some(WorkflowScope::Local),
                    requires_confirmation: *requires_confirmation,
                    sort_order: index as i64,
                    created_at: "2026-10-18T00:00:00Z".to_owned(),
                    updated_at: "2026-10-18T00:00:00Z".to_owned(),
                },
            )
            .collect(),
        sort_order: 0,
        created_at: "2026-10-18T00:00:00Z".to_owned(),
        updated_at: "2026-10-18T00:00:00Z".to_owned(),
    }
}

/// Finished 步骤按定义顺序执行，全部请求固定在启动时捕获的 UI binding 上。
/// @author kongweiguang
#[tokio::test]
async fn execute_workflow_runs_steps_in_order_and_records_workflow_history() {
    let fixture = world(
        definition("unused snippet fixture", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        false,
        false,
    );
    let workflow = workflow(&[("echo first", false), ("echo second", false)]);
    let run = fixture
        .commands
        .start_workflow_run(&workflow)
        .expect("non-empty workflow should start");

    let execution = fixture
        .commands
        .execute_workflow(
            workflow,
            fixture.binding.clone(),
            run,
            false,
            WorkflowExecutionControl::new(),
            CancellationToken::new(),
        )
        .await
        .expect("workflow service should return terminal outcomes");

    assert_eq!(execution.run.state(), WorkflowRunState::Completed);
    assert_eq!(
        execution
            .run
            .results()
            .iter()
            .map(|step| step.state)
            .collect::<Vec<_>>(),
        vec![WorkflowStepState::Succeeded, WorkflowStepState::Succeeded]
    );
    assert_eq!(execution.steps.len(), 2);

    let requests = fixture.sessions.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].command, "echo first");
    assert_eq!(requests[1].command, "echo second");
    for request in &requests {
        assert_eq!(request.session_id, fixture.binding.session_id);
        assert_eq!(request.expected_ui_binding.as_ref(), Some(&fixture.binding));
    }

    let history = fixture.history.drafts();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].source, CommandHistorySource::Workflow);
    assert_eq!(history[0].command, "echo first");
    assert_eq!(history[1].source, CommandHistorySource::Workflow);
    assert_eq!(history[1].command, "echo second");
}

/// 需要确认的步骤在未确认时不得触达终端，确认后沿同一运行继续执行。
/// @author kongweiguang
#[tokio::test]
async fn execute_workflow_confirmation_gate_prevents_terminal_call_until_confirmed() {
    let fixture = world(
        definition("unused snippet fixture", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        false,
        false,
    );
    let workflow = workflow(&[("echo confirmed", true)]);
    let run = fixture
        .commands
        .start_workflow_run(&workflow)
        .expect("non-empty workflow should start");

    let waiting = fixture
        .commands
        .execute_workflow(
            workflow.clone(),
            fixture.binding.clone(),
            run,
            false,
            WorkflowExecutionControl::new(),
            CancellationToken::new(),
        )
        .await
        .expect("unconfirmed workflow should remain reviewable");

    assert_eq!(waiting.run.state(), WorkflowRunState::AwaitingConfirmation);
    assert_eq!(
        waiting.run.results()[0].state,
        WorkflowStepState::AwaitingConfirmation
    );
    assert!(waiting.steps.is_empty());
    assert_eq!(fixture.sessions.call_count(), 0);
    assert!(fixture.history.drafts().is_empty());

    let confirmed = fixture
        .commands
        .execute_workflow(
            workflow,
            fixture.binding.clone(),
            waiting.run,
            true,
            WorkflowExecutionControl::new(),
            CancellationToken::new(),
        )
        .await
        .expect("explicit confirmation should resume the same workflow run");

    assert_eq!(confirmed.run.state(), WorkflowRunState::Completed);
    assert_eq!(fixture.sessions.call_count(), 1);
    assert_eq!(
        fixture.sessions.requests()[0].expected_ui_binding,
        Some(fixture.binding.clone())
    );
    assert_eq!(fixture.history.drafts().len(), 1);
    assert_eq!(
        fixture.history.drafts()[0].source,
        CommandHistorySource::Workflow
    );
}

/// 取消会等待同一 command_id 的 Unknown 回执，确认前不返回也不自动创建重试命令。
/// @author kongweiguang
#[tokio::test]
async fn execute_workflow_cancel_waits_for_the_original_command_receipt() {
    let fixture = world(
        definition("unused snippet fixture", SnippetRisk::Inspect),
        ExecutionMode::WaitForCancellation,
        false,
        false,
    );
    let workflow = workflow(&[("echo cancel", false)]);
    let run = fixture
        .commands
        .start_workflow_run(&workflow)
        .expect("non-empty workflow should start");
    let control = WorkflowExecutionControl::new();
    let started = fixture.sessions.command_started.notified();
    let cancellation_observed = fixture.sessions.cancellation_observed.notified();
    let released = fixture.sessions.cancelled_result_released.notified();
    let (progress_sender, progress_receiver) = WorkflowProgressPublisher::channel(None);
    let commands = fixture.commands.clone();
    let binding = fixture.binding.clone();
    let task_control = control.clone();

    let task = tokio::spawn(async move {
        commands
            .execute_workflow_with_progress(
                WorkflowExecutionRequest {
                    workflow,
                    expected_ui_binding: binding,
                    run,
                    confirmed_current_step: false,
                },
                task_control,
                CancellationToken::new(),
                progress_sender,
            )
            .await
    });

    tokio::time::timeout(GATE_TIMEOUT, started)
        .await
        .expect("workflow should submit one command");
    let running = progress_receiver
        .snapshot()
        .expect("UI must receive progress before the active command finishes");
    assert_eq!(running.state, WorkflowRunState::Running);
    assert_eq!(running.current_step_index, Some(0));
    assert_eq!(running.steps[0].state, WorkflowStepState::Running);
    assert!(running.steps[0].command_id.is_some());
    control.cancel();
    tokio::time::timeout(GATE_TIMEOUT, cancellation_observed)
        .await
        .expect("service should forward cancellation to the active command");
    assert!(
        !task.is_finished(),
        "service must wait for the original command outcome after cancellation"
    );

    fixture.sessions.release_cancelled_result.notify_one();
    tokio::time::timeout(GATE_TIMEOUT, released)
        .await
        .expect("fake terminal should release its original cancellation result");
    let execution = tokio::time::timeout(GATE_TIMEOUT, task)
        .await
        .expect("workflow should finish after the original receipt is released")
        .expect("workflow service task should not panic")
        .expect("cancelled command is represented as an execution result");
    let progress = progress_receiver
        .snapshot()
        .expect("cancellation must publish the final complete progress snapshot");

    assert_eq!(execution.run.state(), WorkflowRunState::Cancelled);
    assert_eq!(execution.run.results()[0].state, WorkflowStepState::Unknown);
    assert_eq!(progress.run_id, execution.run.id());
    assert_eq!(progress.state, WorkflowRunState::Cancelled);
    assert!(progress.revision > running.revision);
    assert_eq!(progress.steps[0].state, WorkflowStepState::Unknown);
    assert!(progress.revision >= 3);
    assert_eq!(execution.steps.len(), 1);
    let request = &fixture.sessions.requests()[0];
    assert_eq!(fixture.sessions.call_count(), 1);
    assert_eq!(execution.steps[0].command_id, request.command_id);
    assert_eq!(
        progress.steps[0].command_id.as_deref(),
        Some(request.command_id.as_str())
    );
    assert!(matches!(
        execution.steps[0].outcome.as_ref(),
        Some(ShellCommandOutcome::Unknown {
            command_id,
            reason: ShellCommandUnknownReason::Cancelled,
        }) if command_id == &request.command_id
    ));
    assert_eq!(fixture.history.drafts().len(), 1);
    assert_eq!(
        fixture.history.drafts()[0].source,
        CommandHistorySource::Workflow
    );
}

/// 单槽观察者不消费也不阻塞命令，确认后继续时保留前序回执和单调 revision。
/// @author kongweiguang
#[tokio::test]
async fn execute_workflow_progress_survives_confirmation_and_slow_observers() {
    let fixture = world(
        definition("unused snippet fixture", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        false,
        false,
    );
    let workflow = workflow(&[("echo first", false), ("echo confirm", true)]);
    let run = fixture.commands.start_workflow_run(&workflow).unwrap();
    let (sender, receiver) = WorkflowProgressPublisher::channel(None);
    let waiting = fixture
        .commands
        .execute_workflow_with_progress(
            WorkflowExecutionRequest {
                workflow: workflow.clone(),
                expected_ui_binding: fixture.binding.clone(),
                run,
                confirmed_current_step: false,
            },
            WorkflowExecutionControl::new(),
            CancellationToken::new(),
            sender,
        )
        .await
        .expect("slow UI must not backpressure terminal execution");
    let before = receiver.snapshot().unwrap();
    assert_eq!(before.state, WorkflowRunState::AwaitingConfirmation);
    assert_eq!(before.current_step_index, Some(1));
    assert_eq!(before.steps[0].state, WorkflowStepState::Succeeded);
    assert_eq!(before.steps[0].exit_code, Some(0));
    assert_eq!(
        before.steps[1].state,
        WorkflowStepState::AwaitingConfirmation
    );
    assert_eq!(fixture.sessions.call_count(), 1);
    let first_command_id = before.steps[0].command_id.clone();
    assert!(first_command_id.is_some());

    let (sender, receiver) = WorkflowProgressPublisher::channel(Some(before.clone()));
    let completed = fixture
        .commands
        .execute_workflow_with_progress(
            WorkflowExecutionRequest {
                workflow,
                expected_ui_binding: fixture.binding.clone(),
                run: waiting.run,
                confirmed_current_step: true,
            },
            WorkflowExecutionControl::new(),
            CancellationToken::new(),
            sender,
        )
        .await
        .expect("confirmation should continue the exact original run");
    let after = receiver.snapshot().unwrap();
    assert_eq!(after.run_id, before.run_id);
    assert_eq!(after.run_id, completed.run.id());
    assert!(after.revision > before.revision);
    assert_eq!(after.state, WorkflowRunState::Completed);
    assert_eq!(after.steps[0].command_id, first_command_id);
    assert_eq!(after.steps[1].state, WorkflowStepState::Succeeded);
    assert_ne!(after.steps[1].command_id, after.steps[0].command_id);
    assert_eq!(fixture.sessions.call_count(), 2);
}

/// 非零退出与准入前拒绝有不同事实；两者都停止后续步骤，只有显式重试可再次执行。
/// @author kongweiguang
#[tokio::test]
async fn execute_workflow_progress_distinguishes_failed_exit_from_not_started() {
    for (mode, expected_state, expected_exit, expected_error) in [
        (
            ExecutionMode::Failed,
            WorkflowStepState::Failed,
            Some(17),
            None,
        ),
        (
            ExecutionMode::Rejected,
            WorkflowStepState::NotStarted,
            None,
            Some(PublicErrorCode::Conflict),
        ),
    ] {
        let fixture = world(
            definition("unused snippet fixture", SnippetRisk::Inspect),
            mode,
            false,
            false,
        );
        let workflow = workflow(&[("echo fail", false), ("echo must not run", false)]);
        let run = fixture.commands.start_workflow_run(&workflow).unwrap();
        let (sender, receiver) = WorkflowProgressPublisher::channel(None);
        let execution = fixture
            .commands
            .execute_workflow_with_progress(
                WorkflowExecutionRequest {
                    workflow,
                    expected_ui_binding: fixture.binding.clone(),
                    run,
                    confirmed_current_step: false,
                },
                WorkflowExecutionControl::new(),
                CancellationToken::new(),
                sender,
            )
            .await
            .expect("failure must return a reviewable execution, not replay the step");
        let progress = receiver.snapshot().unwrap();
        assert_eq!(execution.run.state(), WorkflowRunState::Failed);
        assert_eq!(progress.state, WorkflowRunState::Failed);
        assert_eq!(progress.steps[0].state, expected_state);
        assert_eq!(progress.steps[0].exit_code, expected_exit);
        assert_eq!(progress.steps[0].error_code, expected_error);
        assert_eq!(progress.steps[1].state, WorkflowStepState::Pending);
        assert_eq!(fixture.sessions.call_count(), 1);
        assert_eq!(
            fixture.history.drafts().len(),
            usize::from(expected_exit.is_some())
        );
        let mut run = execution.run;
        fixture
            .commands
            .retry_workflow_step(&mut run)
            .expect("only this explicit user action should make a known failure retryable");
        assert_eq!(fixture.sessions.call_count(), 1);
    }
}

/// 外来命令的成功或 Unknown 回执都不能推进当前步骤，更不能作为安全重试依据。
/// @author kongweiguang
#[tokio::test]
async fn execute_workflow_rejects_mismatched_command_receipts_as_unknown() {
    for mode in [
        ExecutionMode::MismatchedFinished,
        ExecutionMode::MismatchedUnknown,
    ] {
        let fixture = world(
            definition("unused snippet fixture", SnippetRisk::Inspect),
            mode,
            false,
            false,
        );
        let workflow = workflow(&[("echo original", false), ("echo forbidden", false)]);
        let run = fixture.commands.start_workflow_run(&workflow).unwrap();
        let (sender, receiver) = WorkflowProgressPublisher::channel(None);
        let execution = fixture
            .commands
            .execute_workflow_with_progress(
                WorkflowExecutionRequest {
                    workflow,
                    expected_ui_binding: fixture.binding.clone(),
                    run,
                    confirmed_current_step: false,
                },
                WorkflowExecutionControl::new(),
                CancellationToken::new(),
                sender,
            )
            .await
            .expect("mismatched receipt should retain the original command for review");
        let progress = receiver.snapshot().unwrap();
        assert_eq!(execution.run.state(), WorkflowRunState::Failed);
        assert_eq!(progress.steps[0].state, WorkflowStepState::Unknown);
        assert_eq!(progress.steps[0].exit_code, None);
        assert_eq!(
            progress.steps[0].error_code,
            Some(PublicErrorCode::OutcomeUnknown)
        );
        assert_eq!(fixture.sessions.call_count(), 1);
        let request = &fixture.sessions.requests()[0];
        assert_eq!(execution.steps[0].command_id, request.command_id);
        assert!(execution.steps[0].outcome.is_none());
        assert_eq!(
            progress.steps[0].command_id.as_ref(),
            Some(&request.command_id)
        );
        let mut run = execution.run;
        assert!(fixture.commands.retry_workflow_step(&mut run).is_err());
    }
}

/// 取消后到达的成功 shell receipt 仍是权威结果，不得伪装为 Unknown 或重放命令。
/// @author kongweiguang
#[tokio::test]
async fn execute_workflow_cancel_preserves_late_finished_receipt() {
    let fixture = world(
        definition("unused snippet fixture", SnippetRisk::Inspect),
        ExecutionMode::WaitForLateFinishedReceipt,
        false,
        false,
    );
    let workflow = workflow(&[("echo late success", false)]);
    let run = fixture
        .commands
        .start_workflow_run(&workflow)
        .expect("non-empty workflow should start");
    let control = WorkflowExecutionControl::new();
    let started = fixture.sessions.command_started.notified();
    let cancellation_observed = fixture.sessions.cancellation_observed.notified();
    let released = fixture.sessions.cancelled_result_released.notified();
    let (progress_sender, progress_receiver) = WorkflowProgressPublisher::channel(None);
    let commands = fixture.commands.clone();
    let binding = fixture.binding.clone();
    let task_control = control.clone();

    let task = tokio::spawn(async move {
        commands
            .execute_workflow_with_progress(
                WorkflowExecutionRequest {
                    workflow,
                    expected_ui_binding: binding,
                    run,
                    confirmed_current_step: false,
                },
                task_control,
                CancellationToken::new(),
                progress_sender,
            )
            .await
    });

    tokio::time::timeout(GATE_TIMEOUT, started)
        .await
        .expect("workflow should submit one command before cancellation");
    control.cancel();
    tokio::time::timeout(GATE_TIMEOUT, cancellation_observed)
        .await
        .expect("service should forward cancellation to the active command");
    assert!(
        !task.is_finished(),
        "service must wait for the original command receipt after cancellation"
    );

    fixture.sessions.release_cancelled_result.notify_one();
    tokio::time::timeout(GATE_TIMEOUT, released)
        .await
        .expect("fake terminal should release its late successful receipt");
    let execution = tokio::time::timeout(GATE_TIMEOUT, task)
        .await
        .expect("workflow should finish after the original receipt is released")
        .expect("workflow service task should not panic")
        .expect("late finished receipt is a valid execution result");
    let progress = progress_receiver
        .snapshot()
        .expect("late successful receipt must publish the final progress snapshot");

    assert_eq!(execution.run.state(), WorkflowRunState::Completed);
    assert_eq!(
        execution.run.results()[0].state,
        WorkflowStepState::Succeeded
    );
    assert_eq!(execution.run.results()[0].exit_code, Some(0));
    assert_eq!(progress.run_id, execution.run.id());
    assert_eq!(progress.state, WorkflowRunState::Completed);
    assert_eq!(progress.steps[0].state, WorkflowStepState::Succeeded);
    assert_eq!(progress.steps[0].exit_code, Some(0));
    assert!(progress.revision >= 3);
    assert_eq!(execution.steps.len(), 1);
    let request = &fixture.sessions.requests()[0];
    assert_eq!(fixture.sessions.call_count(), 1);
    assert_eq!(execution.steps[0].command_id, request.command_id);
    assert_eq!(
        progress.steps[0].command_id.as_deref(),
        Some(request.command_id.as_str())
    );
    assert!(matches!(
        execution.steps[0].outcome.as_ref(),
        Some(ShellCommandOutcome::Finished {
            command_id,
            exit_code: 0,
        }) if command_id == &request.command_id
    ));
    assert_eq!(fixture.history.drafts().len(), 1);
    assert_eq!(
        fixture.history.drafts()[0].source,
        CommandHistorySource::Workflow
    );
}

/// 取消与第一步成功竞争时保留成功事实，但不得顺势向后续步骤再注入命令。
/// @author kongweiguang
#[tokio::test]
async fn execute_workflow_cancellation_keeps_late_success_but_skips_remaining_steps() {
    let fixture = world(
        definition("unused snippet fixture", SnippetRisk::Inspect),
        ExecutionMode::WaitForLateFinishedReceipt,
        false,
        false,
    );
    let workflow = workflow(&[("echo late", false), ("echo must not run", false)]);
    let run = fixture.commands.start_workflow_run(&workflow).unwrap();
    let control = WorkflowExecutionControl::new();
    let started = fixture.sessions.command_started.notified();
    let observed = fixture.sessions.cancellation_observed.notified();
    let (sender, receiver) = WorkflowProgressPublisher::channel(None);
    let commands = fixture.commands.clone();
    let binding = fixture.binding.clone();
    let task_control = control.clone();
    let task = tokio::spawn(async move {
        commands
            .execute_workflow_with_progress(
                WorkflowExecutionRequest {
                    workflow,
                    expected_ui_binding: binding,
                    run,
                    confirmed_current_step: false,
                },
                task_control,
                CancellationToken::new(),
                sender,
            )
            .await
    });
    tokio::time::timeout(GATE_TIMEOUT, started).await.unwrap();
    control.cancel();
    tokio::time::timeout(GATE_TIMEOUT, observed).await.unwrap();
    fixture.sessions.release_cancelled_result.notify_one();
    let execution = tokio::time::timeout(GATE_TIMEOUT, task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let progress = receiver.snapshot().unwrap();
    assert_eq!(execution.run.state(), WorkflowRunState::Cancelled);
    assert_eq!(progress.steps[0].state, WorkflowStepState::Succeeded);
    assert_eq!(progress.steps[0].exit_code, Some(0));
    assert_eq!(progress.steps[1].state, WorkflowStepState::Skipped);
    assert!(progress.steps[1].command_id.is_none());
    assert_eq!(fixture.sessions.call_count(), 1);
    assert_eq!(fixture.history.drafts().len(), 1);
}

/// 预先取消的运行只发布可解释的跳过状态，不建立 shell lease，也不伪造历史。
/// @author kongweiguang
#[tokio::test]
async fn execute_workflow_cancelled_before_admission_publishes_without_injection() {
    let fixture = world(
        definition("unused snippet fixture", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        false,
        false,
    );
    let workflow = workflow(&[("echo never", false)]);
    let run = fixture.commands.start_workflow_run(&workflow).unwrap();
    let control = WorkflowExecutionControl::new();
    control.cancel();
    let (sender, receiver) = WorkflowProgressPublisher::channel(None);
    let execution = fixture
        .commands
        .execute_workflow_with_progress(
            WorkflowExecutionRequest {
                workflow,
                expected_ui_binding: fixture.binding.clone(),
                run,
                confirmed_current_step: false,
            },
            control,
            CancellationToken::new(),
            sender,
        )
        .await
        .unwrap();
    let progress = receiver.snapshot().unwrap();
    assert_eq!(execution.run.state(), WorkflowRunState::Cancelled);
    assert_eq!(progress.steps[0].state, WorkflowStepState::Skipped);
    assert!(progress.steps[0].command_id.is_none());
    assert_eq!(fixture.sessions.call_count(), 0);
    assert!(fixture.history.drafts().is_empty());
}

/// Unknown 结果停止后续步骤，拒绝原地重试且仍按工作流既有语义记录历史。
/// @author kongweiguang
#[tokio::test]
async fn execute_workflow_unknown_stops_without_retry_or_target_rebinding() {
    let fixture = world(
        definition("unused snippet fixture", SnippetRisk::Inspect),
        ExecutionMode::Unknown,
        false,
        false,
    );
    let workflow = workflow(&[("echo uncertain", false), ("echo must not run", false)]);
    let run = fixture
        .commands
        .start_workflow_run(&workflow)
        .expect("non-empty workflow should start");
    let (progress_sender, progress_receiver) = WorkflowProgressPublisher::channel(None);

    let execution = fixture
        .commands
        .execute_workflow_with_progress(
            WorkflowExecutionRequest {
                workflow,
                expected_ui_binding: fixture.binding.clone(),
                run,
                confirmed_current_step: false,
            },
            WorkflowExecutionControl::new(),
            CancellationToken::new(),
            progress_sender,
        )
        .await
        .expect("unknown shell result should be returned for user review");

    let progress = progress_receiver
        .snapshot()
        .expect("the bounded observer should retain the latest workflow snapshot");

    assert_eq!(execution.run.state(), WorkflowRunState::Failed);
    assert_eq!(execution.run.results()[0].state, WorkflowStepState::Unknown);
    assert_eq!(execution.run.results()[1].state, WorkflowStepState::Pending);
    assert_eq!(progress.run_id, execution.run.id());
    assert_eq!(progress.workflow_id, WORKFLOW_ID);
    assert_eq!(progress.session_id, fixture.binding.session_id);
    assert_eq!(progress.state, WorkflowRunState::Failed);
    assert_eq!(progress.steps[0].state, WorkflowStepState::Unknown);
    assert_eq!(
        progress.steps[0].command_id.as_deref(),
        Some(execution.steps[0].command_id.as_str())
    );
    assert!(progress.revision >= 3);
    let mut retrying_run = execution.run.clone();
    assert!(
        fixture
            .commands
            .retry_workflow_step(&mut retrying_run)
            .is_err()
    );

    let requests = fixture.sessions.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(fixture.sessions.call_count(), 1);
    assert_eq!(requests[0].session_id, fixture.binding.session_id);
    assert_eq!(
        requests[0].expected_ui_binding.as_ref(),
        Some(&fixture.binding)
    );
    assert_eq!(execution.steps.len(), 1);
    assert_eq!(execution.steps[0].command_id, requests[0].command_id);
    assert!(matches!(
        execution.steps[0].outcome.as_ref(),
        Some(ShellCommandOutcome::Unknown {
            command_id,
            reason: ShellCommandUnknownReason::MissingCompletion,
        }) if command_id == &requests[0].command_id
    ));
    let history = fixture.history.drafts();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].source, CommandHistorySource::Workflow);
    assert_eq!(history[0].command, "echo uncertain");
}

/// 快速完成仍返回最后一步快照；进度仅是投影，完成判定始终取原始执行回执。
/// @author kongweiguang
#[tokio::test]
async fn workflow_progress_subscription_returns_final_snapshot_with_execution_receipt() {
    let fixture = world(
        definition("unused snippet fixture", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        false,
        false,
    );
    let workflow = workflow(&[("echo first", false), ("echo second", false)]);
    let run = fixture.commands.start_workflow_run(&workflow).unwrap();
    let (publisher, mut subscription) = WorkflowProgressPublisher::channel(None);
    let execution = fixture.commands.execute_workflow_with_progress(
        WorkflowExecutionRequest {
            workflow,
            expected_ui_binding: fixture.binding.clone(),
            run,
            confirmed_current_step: false,
        },
        WorkflowExecutionControl::new(),
        CancellationToken::new(),
        publisher,
    );
    let mut execution = std::pin::pin!(execution);
    loop {
        match subscription.next(execution.as_mut()).await {
            WorkflowProgressEvent::Progress(progress) => {
                assert_eq!(progress.session_id, fixture.binding.session_id);
            }
            WorkflowProgressEvent::Finished { result, progress } => {
                let execution = result.unwrap();
                let progress = progress.expect("the final receipt must retain its latest snapshot");
                assert_eq!(progress.run_id, execution.run.id());
                assert_eq!(progress.state, WorkflowRunState::Completed);
                assert_eq!(progress.steps[1].state, WorkflowStepState::Succeeded);
                assert_eq!(progress.steps[1].exit_code, Some(0));
                break;
            }
        }
    }
}

/// 发布端关闭只结束观察，不得伪造成功、丢弃 waiter 或让闭流分支不断唤醒。
/// @author kongweiguang
#[tokio::test]
async fn workflow_progress_subscription_closed_stream_waits_for_original_receipt() {
    let (publisher, mut subscription) = WorkflowProgressPublisher::channel(None);
    drop(publisher);
    let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
    let (receipt_sender, receipt_receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let execution = async move {
            started_sender.send(()).unwrap();
            receipt_receiver.await.unwrap()
        };
        let mut execution = std::pin::pin!(execution);
        subscription.next(execution.as_mut()).await
    });
    tokio::time::timeout(GATE_TIMEOUT, started_receiver)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !task.is_finished(),
        "closed progress is not a command receipt"
    );
    receipt_sender
        .send(Err(crate::AppError::cancelled("fixture original receipt")))
        .unwrap();
    let event = tokio::time::timeout(GATE_TIMEOUT, task)
        .await
        .unwrap()
        .unwrap();
    let WorkflowProgressEvent::Finished { result, progress } = event else {
        panic!("closed progress must wait for the original execution receipt");
    };
    assert_eq!(
        result.unwrap_err().public_code(),
        PublicErrorCode::Cancelled
    );
    assert!(progress.is_none());
}

/// UI 观察任务被丢弃时仍释放 executor waiter 的取消护栏，后台 owner 等真实清理回执后才退出。
/// @author kongweiguang
#[tokio::test]
async fn dropping_workflow_progress_observer_keeps_executor_cancellation_ownership() {
    let executor = crate::ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
    let (publisher, mut subscription) = WorkflowProgressPublisher::channel(None);
    let started = std::sync::Arc::new(tokio::sync::Notify::new());
    let cancelled = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let task_started = started.clone();
    let task_cancelled = cancelled.clone();
    let task_release = release.clone();
    let observer_executor = executor.clone();
    let observer = tokio::spawn(async move {
        let execution = observer_executor.run(move |cancellation| async move {
            let _publisher = publisher;
            task_started.notify_one();
            cancellation.cancelled().await;
            task_cancelled.notify_one();
            task_release.notified().await;
            Err(crate::AppError::cancelled("fixture cleanup receipt"))
        });
        let mut execution = std::pin::pin!(execution);
        subscription.next(execution.as_mut()).await
    });
    tokio::time::timeout(GATE_TIMEOUT, started.notified())
        .await
        .unwrap();
    observer.abort();
    assert!(observer.await.unwrap_err().is_cancelled());
    tokio::time::timeout(GATE_TIMEOUT, cancelled.notified())
        .await
        .unwrap();
    assert_eq!(executor.subscribe_state().borrow().active_tasks, 1);
    release.notify_one();
    executor.shutdown().await.unwrap();
    assert_eq!(executor.subscribe_state().borrow().active_tasks, 0);
}
