//! 将工作流状态机编排到带 OSC 133 结果关联的终端命令端口。
//!
//! @author kongweiguang

use std::{future::Future, pin::Pin, time::Duration};

use kerminal_domain::{
    commands::{
        CommandHistoryRecordRequest, CommandHistoryRecordResult, CommandHistorySource,
        CommandWorkflow, WorkflowRun, WorkflowRunState, WorkflowScope, WorkflowStepState,
    },
    sessions::{SessionLifecycle, SessionOwner, SessionTarget},
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::{
    AppError, AppResult, CommandsService, PublicErrorCode,
    execution::{history_target, local_shell, owner_pane, owner_tab, target_cwd, target_host},
};
use crate::sessions::{SessionFilter, ShellCommandOutcome, ShellCommandRequest, UiSessionBinding};

/// 单步超时后只能保留未知结果，避免长时间等待造成窗口退出无法收敛。
const WORKFLOW_STEP_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// 用户取消工作流时通过同一个 token 中断当前终端 lease，不会杀死共享 session。
#[derive(Clone)]
pub struct WorkflowExecutionControl {
    cancellation: CancellationToken,
}

/// 同一轮执行的不可变输入快照；把定义、运行与精确目标归为一体，防止接线时交换目标参数。
#[derive(Clone, Debug)]
pub struct WorkflowExecutionRequest {
    pub workflow: CommandWorkflow,
    pub expected_ui_binding: UiSessionBinding,
    pub run: WorkflowRun,
    pub confirmed_current_step: bool,
}

/// 一轮执行返回完整聚合状态以及本轮每条命令的终端结果与历史写入结果。
#[derive(Clone, Debug)]
pub struct WorkflowExecution {
    pub run: WorkflowRun,
    pub steps: Vec<WorkflowStepExecution>,
}

/// 工作流进度快照始终完整描述同一轮运行，watch 单槽会合并慢 UI 未及时读取的中间更新。
#[derive(Clone, Debug)]
pub struct WorkflowRunProgress {
    pub run_id: String,
    pub revision: u64,
    pub workflow_id: String,
    pub session_id: kerminal_domain::identity::SessionId,
    pub target: SessionTarget,
    pub state: WorkflowRunState,
    pub current_step_index: Option<usize>,
    pub steps: Vec<WorkflowStepProgress>,
}

/// 发布器仅封装应用层的单槽进度通道；UI 不持有 Tokio 类型，也不参与真实执行结果判定。
pub struct WorkflowProgressPublisher {
    sender: watch::Sender<Option<WorkflowRunProgress>>,
}

/// 最新值订阅与执行 waiter 在同一 UI 任务中消费，丢弃 waiter 仍由共享执行器传播取消。
pub struct WorkflowProgressSubscription {
    receiver: watch::Receiver<Option<WorkflowRunProgress>>,
    open: bool,
}

/// 观察更新不取代最终回执；执行完成时附带末次快照，避免快速结束遗漏最后一步状态。
#[derive(Debug)]
pub enum WorkflowProgressEvent {
    Progress(WorkflowRunProgress),
    Finished {
        result: AppResult<WorkflowExecution>,
        progress: Option<WorkflowRunProgress>,
    },
}

impl WorkflowProgressPublisher {
    /// 创建有界单槽观察端口，继续原运行时可携带前次快照以保留 revision 与步骤命令 ID。
    /// @author kongweiguang
    pub fn channel(initial: Option<WorkflowRunProgress>) -> (Self, WorkflowProgressSubscription) {
        let (sender, receiver) = watch::channel(initial);
        (
            Self { sender },
            WorkflowProgressSubscription {
                receiver,
                open: true,
            },
        )
    }
}

impl WorkflowProgressSubscription {
    /// 复制当前完整投影，不暴露 watch 借用或把进度存在误判为命令成功。
    /// @author kongweiguang
    pub fn snapshot(&self) -> Option<WorkflowRunProgress> {
        self.receiver.borrow().clone()
    }

    /// 优先返回真实执行回执；通道关闭后只等原 waiter，避免闭流热循环或意外丢弃执行。
    /// 不创建任务和 runtime，UI 销毁时丢弃此 future 仍会释放外层执行 waiter 的取消护栏。
    /// @author kongweiguang
    pub async fn next<F>(&mut self, mut execution: Pin<&mut F>) -> WorkflowProgressEvent
    where
        F: Future<Output = AppResult<WorkflowExecution>> + ?Sized,
    {
        loop {
            if !self.open {
                return WorkflowProgressEvent::Finished {
                    result: execution.await,
                    progress: self.receiver.borrow_and_update().clone(),
                };
            }
            tokio::select! {
                biased;
                result = &mut execution => {
                    return WorkflowProgressEvent::Finished {
                        result,
                        progress: self.receiver.borrow_and_update().clone(),
                    };
                }
                changed = self.receiver.changed() => {
                    match changed {
                        Ok(()) => {
                            if let Some(progress) = self.receiver.borrow_and_update().clone() {
                                return WorkflowProgressEvent::Progress(progress);
                            }
                        }
                        Err(_) => self.open = false,
                    }
                }
            }
        }
    }
}

/// 当前定义步骤在完整进度快照中的只读投影，保留真实 shell 退出码与未知状态。
#[derive(Clone, Debug)]
pub struct WorkflowStepProgress {
    pub step_id: String,
    pub title: String,
    pub state: WorkflowStepState,
    pub exit_code: Option<i32>,
    pub message: Option<String>,
    pub command_id: Option<String>,
    pub error_code: Option<PublicErrorCode>,
}

/// 逐步报告 shell 结果、执行前拒绝原因及可能失败的历史记录。
#[derive(Clone, Debug)]
pub struct WorkflowStepExecution {
    pub step_id: String,
    pub command_id: String,
    pub outcome: Option<ShellCommandOutcome>,
    pub error_code: Option<PublicErrorCode>,
    pub history: Option<CommandHistoryRecordResult>,
    pub history_error_code: Option<PublicErrorCode>,
}

impl WorkflowExecutionControl {
    /// 创建与 UI 当前工作流绑定的取消句柄，避免对 session 或应用全局 token 发取消。
    pub fn new() -> Self {
        Self {
            cancellation: CancellationToken::new(),
        }
    }

    /// 请求停止当前工作流；正在执行的命令由 terminal lease 返回 Unknown 后再结束。
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// 允许 UI 在切换 session 或卸载面板前展示即时取消状态。
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

impl Default for WorkflowExecutionControl {
    /// 每个执行句柄使用新 token，避免默认状态意外共享工作流取消范围。
    fn default() -> Self {
        Self::new()
    }
}

impl CommandsService {
    /// 根据用户选中的不可变定义快照创建运行聚合，空流程不会进入执行器。
    pub fn start_workflow_run(&self, workflow: &CommandWorkflow) -> AppResult<WorkflowRun> {
        WorkflowRun::start(uuid::Uuid::new_v4().to_string(), workflow)
            .map_err(|_| AppError::invalid("工作流没有可执行步骤"))
    }

    /// 只重试明确非零退出或终端确认未注入的当前步骤；Unknown 必须人工检查后新建运行。
    pub fn retry_workflow_step(&self, run: &mut WorkflowRun) -> AppResult<()> {
        run.retry_failed_current_step()
            .map_err(|_| AppError::conflict("只有明确失败或未注入的工作流步骤可以重试"))
    }

    /// 逐步执行同一 UI session 的工作流，整轮复用启动时捕获的 binding，阻止重连后转投新代。
    /// 会话 ID 只从精确 binding 取得，避免两个独立参数表达不同执行目标。
    /// @author kongweiguang
    pub async fn execute_workflow(
        &self,
        workflow: CommandWorkflow,
        expected_ui_binding: UiSessionBinding,
        run: WorkflowRun,
        confirmed_current_step: bool,
        control: WorkflowExecutionControl,
        cancellation: CancellationToken,
    ) -> AppResult<WorkflowExecution> {
        self.execute_workflow_inner(
            WorkflowExecutionRequest {
                workflow,
                expected_ui_binding,
                run,
                confirmed_current_step,
            },
            control,
            cancellation,
            None,
        )
        .await
    }

    /// 用不可变请求绑定定义、目标和原运行，单槽进度端口不参与执行结果与取消的判定。
    /// @author kongweiguang
    pub async fn execute_workflow_with_progress(
        &self,
        request: WorkflowExecutionRequest,
        control: WorkflowExecutionControl,
        cancellation: CancellationToken,
        progress: WorkflowProgressPublisher,
    ) -> AppResult<WorkflowExecution> {
        self.execute_workflow_inner(request, control, cancellation, Some(progress))
            .await
    }

    /// 执行器只维护一份状态机；归集请求后统一校验定义、运行与目标，不把观察端口混入业务输入。
    /// @author kongweiguang
    async fn execute_workflow_inner(
        &self,
        request: WorkflowExecutionRequest,
        control: WorkflowExecutionControl,
        cancellation: CancellationToken,
        progress: Option<WorkflowProgressPublisher>,
    ) -> AppResult<WorkflowExecution> {
        let WorkflowExecutionRequest {
            workflow,
            expected_ui_binding,
            run,
            confirmed_current_step,
        } = request;
        if run.workflow_id() != workflow.id
            || run.results().len() != workflow.steps.len()
            || run
                .results()
                .iter()
                .zip(&workflow.steps)
                .any(|(result, step)| result.step_id != step.id)
        {
            return Err(AppError::conflict("工作流运行实例与定义不匹配"));
        }
        let session_id = expected_ui_binding.session_id.clone();
        let session = self
            .sessions
            .list(SessionFilter::default())
            .await?
            .into_iter()
            .find(|session| session.session_id == session_id)
            .ok_or_else(|| AppError::not_found("指定终端会话不存在"))?;
        if session.lifecycle != SessionLifecycle::Running {
            return Err(AppError::conflict("指定终端会话当前不可执行工作流"));
        }
        if expected_ui_binding.owner != session.owner
            || !matches!(&expected_ui_binding.owner, SessionOwner::Ui { .. })
        {
            return Err(AppError::conflict(
                "工作流目标已变化，请重新选择终端并启动新的运行",
            ));
        }
        validate_workflow_target(&workflow, &session.target)?;

        let mut result = WorkflowExecution {
            run,
            steps: Vec::new(),
        };
        let previous_progress = progress
            .as_ref()
            .and_then(|publisher| publisher.sender.borrow().clone())
            .filter(|snapshot| {
                snapshot.run_id == result.run.id()
                    && snapshot.workflow_id == workflow.id
                    && snapshot.session_id == session_id
                    && snapshot.target == session.target
                    && snapshot.steps.len() == workflow.steps.len()
                    && snapshot
                        .steps
                        .iter()
                        .zip(&workflow.steps)
                        .all(|(previous, step)| previous.step_id == step.id)
            });
        let mut progress_revision = previous_progress
            .as_ref()
            .map(|snapshot| snapshot.revision)
            .unwrap_or(0);
        let mut command_ids = workflow
            .steps
            .iter()
            .enumerate()
            .map(|(index, _)| {
                previous_progress
                    .as_ref()
                    .and_then(|snapshot| snapshot.steps.get(index))
                    .and_then(|step| step.command_id.clone())
            })
            .collect::<Vec<_>>();
        let mut error_codes = workflow
            .steps
            .iter()
            .enumerate()
            .map(|(index, _)| {
                previous_progress
                    .as_ref()
                    .and_then(|snapshot| snapshot.steps.get(index))
                    .and_then(|step| step.error_code)
            })
            .collect::<Vec<_>>();
        publish_workflow_progress(
            progress.as_ref(),
            &mut progress_revision,
            &result.run,
            &workflow,
            &session,
            &command_ids,
            &error_codes,
        );
        if is_cancelled(&control, &cancellation) {
            cancel_active_run(&mut result.run);
            publish_workflow_progress(
                progress.as_ref(),
                &mut progress_revision,
                &result.run,
                &workflow,
                &session,
                &command_ids,
                &error_codes,
            );
            return Ok(result);
        }

        if result.run.state() == WorkflowRunState::AwaitingConfirmation {
            if !confirmed_current_step {
                return Ok(result);
            }
            result
                .run
                .confirm_current_step()
                .map_err(|_| AppError::conflict("工作流确认状态已经变化"))?;
            publish_workflow_progress(
                progress.as_ref(),
                &mut progress_revision,
                &result.run,
                &workflow,
                &session,
                &command_ids,
                &error_codes,
            );
        } else if result.run.state() == WorkflowRunState::Failed
            || result.run.state() == WorkflowRunState::Cancelled
            || result.run.state() == WorkflowRunState::Completed
        {
            return Ok(result);
        }

        loop {
            if is_cancelled(&control, &cancellation) {
                cancel_active_run(&mut result.run);
                publish_workflow_progress(
                    progress.as_ref(),
                    &mut progress_revision,
                    &result.run,
                    &workflow,
                    &session,
                    &command_ids,
                    &error_codes,
                );
                return Ok(result);
            }
            let index = result.run.current_step();
            let step = workflow
                .steps
                .get(index)
                .ok_or_else(|| AppError::conflict("工作流当前步骤不存在"))?;
            let current_state = result
                .run
                .current_result()
                .map(|step| step.state)
                .ok_or_else(|| AppError::conflict("工作流步骤状态不存在"))?;

            if step.requires_confirmation && current_state == WorkflowStepState::Pending {
                result
                    .run
                    .await_confirmation()
                    .map_err(|_| AppError::conflict("工作流确认状态无法推进"))?;
                publish_workflow_progress(
                    progress.as_ref(),
                    &mut progress_revision,
                    &result.run,
                    &workflow,
                    &session,
                    &command_ids,
                    &error_codes,
                );
                return Ok(result);
            }
            if current_state == WorkflowStepState::Pending {
                result
                    .run
                    .begin_current_step()
                    .map_err(|_| AppError::conflict("工作流当前步骤无法启动"))?;
            } else if current_state != WorkflowStepState::Running {
                return Err(AppError::conflict("工作流当前步骤状态不允许执行"));
            }

            let command_id = uuid::Uuid::new_v4().to_string();
            command_ids[index] = Some(command_id.clone());
            error_codes[index] = None;
            publish_workflow_progress(
                progress.as_ref(),
                &mut progress_revision,
                &result.run,
                &workflow,
                &session,
                &command_ids,
                &error_codes,
            );
            let request = ShellCommandRequest {
                session_id: session_id.clone(),
                expected_ui_binding: Some(expected_ui_binding.clone()),
                command_id: command_id.clone(),
                command: step.command.clone(),
                timeout: WORKFLOW_STEP_TIMEOUT,
            };
            let shell_cancellation = CancellationToken::new();
            let shell_execution = self
                .sessions
                .execute_shell_command(request, shell_cancellation.clone());
            tokio::pin!(shell_execution);
            let execution_result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    shell_cancellation.cancel();
                    (&mut shell_execution).await
                }
                _ = control.cancellation.cancelled() => {
                    shell_cancellation.cancel();
                    (&mut shell_execution).await
                }
                outcome = &mut shell_execution => outcome,
            };

            let execution_result = execution_result
                .and_then(|outcome| validate_workflow_receipt(outcome, &command_id));
            match execution_result {
                Ok(outcome @ ShellCommandOutcome::Finished { exit_code, .. }) => {
                    result
                        .run
                        .complete_current_step(exit_code)
                        .map_err(|_| AppError::conflict("工作流命令结果无法应用"))?;
                    publish_workflow_progress(
                        progress.as_ref(),
                        &mut progress_revision,
                        &result.run,
                        &workflow,
                        &session,
                        &command_ids,
                        &error_codes,
                    );
                    result.steps.push(
                        self.record_step_result(
                            &session,
                            &step.id,
                            &command_id,
                            step.command.clone(),
                            Some(outcome),
                            None,
                        )
                        .await,
                    );
                    if exit_code != 0 || result.run.state() == WorkflowRunState::Completed {
                        return Ok(result);
                    }
                }
                Ok(outcome @ ShellCommandOutcome::Unknown { .. }) => {
                    let was_cancelled = matches!(
                        &outcome,
                        ShellCommandOutcome::Unknown {
                            reason: crate::sessions::ShellCommandUnknownReason::Cancelled,
                            ..
                        }
                    ) && is_cancelled(&control, &cancellation);
                    if was_cancelled {
                        cancel_active_run(&mut result.run);
                    } else {
                        result
                            .run
                            .mark_current_step_unknown()
                            .map_err(|_| AppError::conflict("工作流未知结果无法应用"))?;
                    }
                    publish_workflow_progress(
                        progress.as_ref(),
                        &mut progress_revision,
                        &result.run,
                        &workflow,
                        &session,
                        &command_ids,
                        &error_codes,
                    );
                    result.steps.push(
                        self.record_step_result(
                            &session,
                            &step.id,
                            &command_id,
                            step.command.clone(),
                            Some(outcome),
                            None,
                        )
                        .await,
                    );
                    return Ok(result);
                }
                Err(error)
                    if matches!(
                        error.public_code(),
                        PublicErrorCode::CapabilityNotSupported | PublicErrorCode::Conflict
                    ) =>
                {
                    result
                        .run
                        .mark_current_step_not_started()
                        .map_err(|_| AppError::conflict("未注入的工作流步骤无法标记"))?;
                    error_codes[index] = Some(error.public_code());
                    publish_workflow_progress(
                        progress.as_ref(),
                        &mut progress_revision,
                        &result.run,
                        &workflow,
                        &session,
                        &command_ids,
                        &error_codes,
                    );
                    result.steps.push(WorkflowStepExecution {
                        step_id: step.id.clone(),
                        command_id,
                        outcome: None,
                        error_code: Some(error.public_code()),
                        history: None,
                        history_error_code: None,
                    });
                    return Ok(result);
                }
                Err(error) => {
                    result
                        .run
                        .mark_current_step_unknown()
                        .map_err(|_| AppError::conflict("工作流执行错误无法标记"))?;
                    error_codes[index] = Some(error.public_code());
                    publish_workflow_progress(
                        progress.as_ref(),
                        &mut progress_revision,
                        &result.run,
                        &workflow,
                        &session,
                        &command_ids,
                        &error_codes,
                    );
                    result.steps.push(
                        self.record_step_result(
                            &session,
                            &step.id,
                            &command_id,
                            step.command.clone(),
                            None,
                            Some(error.public_code()),
                        )
                        .await,
                    );
                    return Ok(result);
                }
            }
        }
    }

    /// 尝试把已注入命令写入历史；失败只降级记录状态，不反转 shell 已报告的执行事实。
    async fn record_step_result(
        &self,
        session: &kerminal_domain::sessions::SessionInfo,
        step_id: &str,
        command_id: &str,
        command: String,
        outcome: Option<ShellCommandOutcome>,
        error_code: Option<PublicErrorCode>,
    ) -> WorkflowStepExecution {
        let history = self
            .record_history(CommandHistoryRecordRequest {
                command,
                source: CommandHistorySource::Workflow,
                target: history_target(&session.target),
                record: Some(true),
                session_id: Some(session.session_id.clone()),
                pane_id: owner_pane(&session.owner),
                tab_id: owner_tab(&session.owner),
                profile_id: None,
                remote_host_id: target_host(&session.target),
                cwd: target_cwd(&session.target),
                shell: local_shell(&session.target),
            })
            .await;
        match history {
            Ok(history) => WorkflowStepExecution {
                step_id: step_id.to_owned(),
                command_id: command_id.to_owned(),
                outcome,
                error_code,
                history: Some(history),
                history_error_code: None,
            },
            Err(error) => WorkflowStepExecution {
                step_id: step_id.to_owned(),
                command_id: command_id.to_owned(),
                outcome,
                error_code,
                history: None,
                history_error_code: Some(error.public_code()),
            },
        }
    }
}

/// 不同 command_id 的迟到回执不能证明本步成功或未执行；按未知结果停止，禁止自动重放。
/// @author kongweiguang
fn validate_workflow_receipt(
    outcome: ShellCommandOutcome,
    expected_command_id: &str,
) -> AppResult<ShellCommandOutcome> {
    let reported_command_id = match &outcome {
        ShellCommandOutcome::Finished { command_id, .. }
        | ShellCommandOutcome::Unknown { command_id, .. } => command_id,
    };
    if reported_command_id != expected_command_id {
        return Err(AppError::outcome_unknown(
            "工作流命令回执不匹配，请核对原终端结果",
        ));
    }
    Ok(outcome)
}

/// 从运行聚合复制有界业务快照；revision 让 UI 丢弃同一 run 内迟到的较旧投影。
/// @author kongweiguang
fn publish_workflow_progress(
    sender: Option<&WorkflowProgressPublisher>,
    revision: &mut u64,
    run: &WorkflowRun,
    workflow: &CommandWorkflow,
    session: &kerminal_domain::sessions::SessionInfo,
    command_ids: &[Option<String>],
    error_codes: &[Option<PublicErrorCode>],
) {
    let Some(sender) = sender else {
        return;
    };
    *revision = (*revision).saturating_add(1);
    let steps = run
        .results()
        .iter()
        .enumerate()
        .map(|(index, result)| {
            let title = workflow
                .steps
                .get(index)
                .filter(|step| step.id == result.step_id)
                .map(|step| step.title.clone())
                .unwrap_or_else(|| result.step_id.clone());
            WorkflowStepProgress {
                step_id: result.step_id.clone(),
                title,
                state: result.state,
                exit_code: result.exit_code,
                message: result.message.clone(),
                command_id: command_ids.get(index).cloned().flatten(),
                error_code: error_codes.get(index).cloned().flatten(),
            }
        })
        .collect();
    sender.sender.send_replace(Some(WorkflowRunProgress {
        run_id: run.id().to_owned(),
        revision: *revision,
        workflow_id: run.workflow_id().to_owned(),
        session_id: session.session_id.clone(),
        target: session.target.clone(),
        state: run.state(),
        current_step_index: run.current_result().map(|_| run.current_step()),
        steps,
    }));
}

/// 检查工作流和每步的协议范围，命中不支持的平台时在任何命令注入前失败。
fn validate_workflow_target(workflow: &CommandWorkflow, target: &SessionTarget) -> AppResult<()> {
    let supported_scope = match target {
        SessionTarget::Local(_) => Some(WorkflowScope::Local),
        SessionTarget::Ssh(_) => Some(WorkflowScope::Ssh),
        SessionTarget::Telnet(_) | SessionTarget::Serial(_) | SessionTarget::Container(_) => None,
    };
    if !scope_matches(workflow.scope, supported_scope) {
        return Err(AppError::capability_not_supported("workflow target scope"));
    }
    if workflow
        .steps
        .iter()
        .filter_map(|step| step.scope)
        .any(|scope| !scope_matches(scope, supported_scope))
    {
        return Err(AppError::capability_not_supported(
            "workflow step target scope",
        ));
    }
    Ok(())
}

/// Any 工作流适用于全部终端；Local/SSH 约束在 Telnet、串口和容器终端前 fail closed。
fn scope_matches(scope: WorkflowScope, target_scope: Option<WorkflowScope>) -> bool {
    match scope {
        WorkflowScope::Any => true,
        WorkflowScope::Local | WorkflowScope::Ssh => target_scope == Some(scope),
    }
}

/// 检查两层取消令牌，保持用户停止和应用退出拥有同一安全路径。
fn is_cancelled(control: &WorkflowExecutionControl, cancellation: &CancellationToken) -> bool {
    control.cancellation.is_cancelled() || cancellation.is_cancelled()
}

/// 状态机只允许非终态取消；重复取消视为幂等状态读取。
fn cancel_active_run(run: &mut WorkflowRun) {
    if !matches!(
        run.state(),
        WorkflowRunState::Completed | WorkflowRunState::Failed | WorkflowRunState::Cancelled
    ) {
        let _ = run.cancel();
    }
}
