//! 有序多步命令工作流及其执行状态。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};

/// 工作流适用范围。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowScope {
    #[default]
    Any,
    Local,
    Ssh,
}

/// 工作流中的可复用命令步骤。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandWorkflowStep {
    pub id: String,
    pub title: String,
    pub command: String,
    pub description: Option<String>,
    pub scope: Option<WorkflowScope>,
    pub requires_confirmation: bool,
    pub sort_order: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// 多步工作流定义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandWorkflow {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub scope: WorkflowScope,
    pub steps: Vec<CommandWorkflowStep>,
    pub sort_order: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// 新建或更新步骤的输入结构。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowStepInput {
    pub id: Option<String>,
    pub title: String,
    pub command: String,
    pub description: Option<String>,
    pub scope: Option<WorkflowScope>,
    #[serde(default)]
    pub requires_confirmation: bool,
}

/// 更新时沿用旧 id，其余字段在 application boundary 统一规范化。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowUpdateRequest {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub scope: WorkflowScope,
    pub sort_order: i64,
    pub steps: Vec<WorkflowStepInput>,
}

/// 列表查询可以组合关键词、scope 和标签。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowListRequest {
    pub query: Option<String>,
    pub scope: Option<WorkflowScope>,
    pub tag: Option<String>,
}

/// 工作流状态机；每个步骤单独受确认和执行结果驱动。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowRunState {
    Ready,
    Running,
    AwaitingConfirmation,
    Completed,
    Failed,
    Cancelled,
}

/// 工作流执行中的一个步骤结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowStepResult {
    pub step_id: String,
    pub state: WorkflowStepState,
    pub exit_code: Option<i32>,
    pub message: Option<String>,
}

/// 步骤状态保留部分完成，失败后不得伪报整个工作流成功。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowStepState {
    Pending,
    AwaitingConfirmation,
    Running,
    Succeeded,
    Failed,
    NotStarted,
    Unknown,
    Skipped,
}

/// 一次具体执行实例；状态字段私有，只有合法迁移方法可以推进执行历史。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRun {
    id: String,
    workflow_id: String,
    state: WorkflowRunState,
    current_step: usize,
    results: Vec<WorkflowStepResult>,
}

impl WorkflowRun {
    /// 拒绝空流程并从 Ready 开始，首步必须显式进入 Running 或确认态后才能完成。
    pub fn start(
        id: impl Into<String>,
        workflow: &CommandWorkflow,
    ) -> Result<Self, WorkflowTransitionError> {
        if workflow.steps.is_empty() {
            return Err(WorkflowTransitionError::EmptyWorkflow);
        }
        Ok(Self {
            id: id.into(),
            workflow_id: workflow.id.clone(),
            state: WorkflowRunState::Ready,
            current_step: 0,
            results: workflow
                .steps
                .iter()
                .map(|step| WorkflowStepResult {
                    step_id: step.id.clone(),
                    state: WorkflowStepState::Pending,
                    exit_code: None,
                    message: None,
                })
                .collect(),
        })
    }

    /// 读取执行标识供应用层关联取消 token 和结果，不开放身份写入口。
    pub fn id(&self) -> &str {
        &self.id
    }

    /// 读取来源定义标识，工作流更新不会改变已启动实例的来源。
    pub fn workflow_id(&self) -> &str {
        &self.workflow_id
    }

    /// 读取生命周期供界面和用例协调下一步动作。
    pub const fn state(&self) -> WorkflowRunState {
        self.state
    }

    /// 读取当前步骤位置，具体结果仍通过只读切片访问。
    pub const fn current_step(&self) -> usize {
        self.current_step
    }

    /// 只读结果让调用方渲染执行历史而不能篡改聚合状态。
    pub fn results(&self) -> &[WorkflowStepResult] {
        &self.results
    }

    /// 当前步骤可能在终态或无效状态下不存在，查询始终返回 Option。
    pub fn current_result(&self) -> Option<&WorkflowStepResult> {
        self.results.get(self.current_step)
    }

    /// 非确认步骤由编排器显式启动，Pending 结果不能被直接标记成功。
    pub fn begin_current_step(&mut self) -> Result<(), WorkflowTransitionError> {
        if !matches!(
            self.state,
            WorkflowRunState::Ready | WorkflowRunState::Running
        ) {
            return Err(WorkflowTransitionError::InvalidState);
        }
        let result = self
            .results
            .get_mut(self.current_step)
            .ok_or(WorkflowTransitionError::NoCurrentStep)?;
        if result.state != WorkflowStepState::Pending {
            return Err(WorkflowTransitionError::InvalidState);
        }
        result.state = WorkflowStepState::Running;
        self.state = WorkflowRunState::Running;
        Ok(())
    }

    /// 等待显式确认时不前移步骤，拒绝后续执行绕过风险提示。
    pub fn await_confirmation(&mut self) -> Result<(), WorkflowTransitionError> {
        if !matches!(
            self.state,
            WorkflowRunState::Ready | WorkflowRunState::Running
        ) {
            return Err(WorkflowTransitionError::InvalidState);
        }
        let result = self
            .results
            .get_mut(self.current_step)
            .ok_or(WorkflowTransitionError::NoCurrentStep)?;
        if result.state != WorkflowStepState::Pending {
            return Err(WorkflowTransitionError::InvalidState);
        }
        result.state = WorkflowStepState::AwaitingConfirmation;
        self.state = WorkflowRunState::AwaitingConfirmation;
        Ok(())
    }

    /// 确认只恢复当前等待步骤，查询用 get_mut 避免损坏索引导致 panic。
    pub fn confirm_current_step(&mut self) -> Result<(), WorkflowTransitionError> {
        if self.state != WorkflowRunState::AwaitingConfirmation {
            return Err(WorkflowTransitionError::InvalidState);
        }
        let result = self
            .results
            .get_mut(self.current_step)
            .ok_or(WorkflowTransitionError::NoCurrentStep)?;
        if result.state != WorkflowStepState::AwaitingConfirmation {
            return Err(WorkflowTransitionError::InvalidState);
        }
        result.state = WorkflowStepState::Running;
        self.state = WorkflowRunState::Running;
        Ok(())
    }

    /// 只接受 Running 步骤的退出结果；失败立即终止后续执行。
    pub fn complete_current_step(&mut self, exit_code: i32) -> Result<(), WorkflowTransitionError> {
        if self.state != WorkflowRunState::Running {
            return Err(WorkflowTransitionError::InvalidState);
        }
        let result = self
            .results
            .get_mut(self.current_step)
            .ok_or(WorkflowTransitionError::NoCurrentStep)?;
        if result.state != WorkflowStepState::Running {
            return Err(WorkflowTransitionError::InvalidState);
        }
        result.exit_code = Some(exit_code);
        result.state = if exit_code == 0 {
            WorkflowStepState::Succeeded
        } else {
            WorkflowStepState::Failed
        };
        if exit_code != 0 {
            self.state = WorkflowRunState::Failed;
        } else if self.current_step + 1 == self.results.len() {
            self.state = WorkflowRunState::Completed;
        } else {
            self.current_step += 1;
        }
        Ok(())
    }

    /// 断线或缺少 shell 完成信号时记录未知结果，避免把发送确认误当作退出状态。
    pub fn mark_current_step_unknown(&mut self) -> Result<(), WorkflowTransitionError> {
        if self.state != WorkflowRunState::Running {
            return Err(WorkflowTransitionError::InvalidState);
        }
        let result = self
            .results
            .get_mut(self.current_step)
            .ok_or(WorkflowTransitionError::NoCurrentStep)?;
        if result.state != WorkflowStepState::Running {
            return Err(WorkflowTransitionError::InvalidState);
        }
        result.state = WorkflowStepState::Unknown;
        result.exit_code = None;
        result.message = Some("终端未提供可靠的命令完成结果".to_owned());
        self.state = WorkflowRunState::Failed;
        Ok(())
    }

    /// 终端在写入前拒绝命令时保留“未执行”证据，供用户显式重试而不误报退出码。
    pub fn mark_current_step_not_started(&mut self) -> Result<(), WorkflowTransitionError> {
        if self.state != WorkflowRunState::Running {
            return Err(WorkflowTransitionError::InvalidState);
        }
        let result = self
            .results
            .get_mut(self.current_step)
            .ok_or(WorkflowTransitionError::NoCurrentStep)?;
        if result.state != WorkflowStepState::Running {
            return Err(WorkflowTransitionError::InvalidState);
        }
        result.state = WorkflowStepState::NotStarted;
        result.exit_code = None;
        result.message = Some("终端在注入命令前拒绝了执行".to_owned());
        self.state = WorkflowRunState::Failed;
        Ok(())
    }

    /// 只允许用户显式重试已知非零退出的当前步骤，不自动重放可能产生副作用的命令。
    pub fn retry_failed_current_step(&mut self) -> Result<(), WorkflowTransitionError> {
        if self.state != WorkflowRunState::Failed {
            return Err(WorkflowTransitionError::InvalidState);
        }
        let result = self
            .results
            .get_mut(self.current_step)
            .ok_or(WorkflowTransitionError::NoCurrentStep)?;
        let retryable = match result.state {
            WorkflowStepState::Failed => result.exit_code.is_some_and(|code| code != 0),
            WorkflowStepState::NotStarted => result.exit_code.is_none(),
            _ => false,
        };
        if !retryable {
            return Err(WorkflowTransitionError::InvalidState);
        }
        result.state = WorkflowStepState::Running;
        result.exit_code = None;
        result.message = None;
        self.state = WorkflowRunState::Running;
        Ok(())
    }

    /// 用户取消保留成功步骤；取消中的 Running 步骤标记未知，避免伪造可安全重试的失败。
    pub fn cancel(&mut self) -> Result<(), WorkflowTransitionError> {
        if matches!(
            self.state,
            WorkflowRunState::Completed | WorkflowRunState::Failed | WorkflowRunState::Cancelled
        ) {
            return Err(WorkflowTransitionError::InvalidState);
        }
        self.state = WorkflowRunState::Cancelled;
        if let Some(result) = self.results.get_mut(self.current_step) {
            match result.state {
                WorkflowStepState::Running => {
                    result.state = WorkflowStepState::Unknown;
                    result.message = Some("取消时未能确认命令退出结果".to_owned());
                }
                WorkflowStepState::Pending | WorkflowStepState::AwaitingConfirmation => {
                    result.state = WorkflowStepState::Skipped;
                }
                WorkflowStepState::Succeeded
                | WorkflowStepState::Failed
                | WorkflowStepState::NotStarted
                | WorkflowStepState::Unknown
                | WorkflowStepState::Skipped => {}
            }
        }
        Ok(())
    }
}

/// 工作流定义的输入约束错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowValidationError {
    EmptyTitle,
    TitleTooLong,
    DescriptionTooLong,
    TooManyTags,
    TagTooLong,
    EmptySteps,
    TooManySteps,
    EmptyStepCommand,
    StepCommandTooLong,
    StepCommandBytesTooLong,
    StepCommandControlCharacter,
}

/// 工作流状态转换错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowTransitionError {
    InvalidState,
    NoCurrentStep,
    EmptyWorkflow,
}

/// 校验工作流并限制单次编排规模，防止错误文件造成无限/空流程。
pub fn validate_workflow(
    title: &str,
    description: Option<&str>,
    tags: &[String],
    steps: &[WorkflowStepInput],
) -> Result<(), WorkflowValidationError> {
    if title.trim().is_empty() {
        return Err(WorkflowValidationError::EmptyTitle);
    }
    if title.chars().count() > 80 {
        return Err(WorkflowValidationError::TitleTooLong);
    }
    if description.is_some_and(|text| text.chars().count() > 500) {
        return Err(WorkflowValidationError::DescriptionTooLong);
    }
    if tags.len() > 12 {
        return Err(WorkflowValidationError::TooManyTags);
    }
    if tags.iter().any(|tag| tag.chars().count() > 32) {
        return Err(WorkflowValidationError::TagTooLong);
    }
    if steps.is_empty() {
        return Err(WorkflowValidationError::EmptySteps);
    }
    if steps.len() > 40 {
        return Err(WorkflowValidationError::TooManySteps);
    }
    for step in steps {
        if step.command.trim().is_empty() {
            return Err(WorkflowValidationError::EmptyStepCommand);
        }
        if step.command.chars().count() > 8_000 {
            return Err(WorkflowValidationError::StepCommandTooLong);
        }
        if step.command.len() > 32_000 {
            return Err(WorkflowValidationError::StepCommandBytesTooLong);
        }
        if step
            .command
            .chars()
            .any(|character| matches!(character, '\r' | '\0' | '\u{1b}'))
        {
            return Err(WorkflowValidationError::StepCommandControlCharacter);
        }
    }
    Ok(())
}
