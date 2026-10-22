// @author kongweiguang

use kerminal_domain::commands::{
    CommandHistoryTarget, CommandWorkflow, CommandWorkflowStep, NormalizedCommandHistory,
    SnippetDefaultAction, SnippetDefinition, SnippetRisk, SnippetScope, SnippetVariable,
    SnippetVariableKind, SnippetVariableRenderStrategy, WorkflowRun, WorkflowRunState,
    WorkflowStepState, WorkflowTransitionError,
};

/// 创建最小且有效的片段定义，避免各断言重复构造无关目录元数据。
fn snippet(command: &str, variables: Vec<SnippetVariable>) -> SnippetDefinition {
    SnippetDefinition {
        id: "snippet-1".to_owned(),
        title: "Inspect service".to_owned(),
        command: command.to_owned(),
        description: None,
        tags: Vec::new(),
        sort_order: 0,
        scope: SnippetScope::Any,
        category: None,
        risk: SnippetRisk::Inspect,
        default_action: SnippetDefaultAction::Insert,
        variables,
        context_bindings: Vec::new(),
        derived_from: None,
    }
}

/// 创建用于占位符校验的普通文本参数，不引入敏感默认值或 raw 渲染。
fn text_variable(name: &str) -> SnippetVariable {
    SnippetVariable {
        name: name.to_owned(),
        label: name.to_owned(),
        description: String::new(),
        kind: SnippetVariableKind::Text,
        required: true,
        default_value: None,
        suggestions: Vec::new(),
        validation: None,
        render_strategy: SnippetVariableRenderStrategy::ShellArg,
        sensitive: false,
    }
}

/// 创建带一个待执行步骤的工作流，确保测试只考察状态机约束。
fn workflow(step_requires_confirmation: bool) -> CommandWorkflow {
    CommandWorkflow {
        id: "workflow-1".to_owned(),
        title: "Inspect".to_owned(),
        description: None,
        tags: Vec::new(),
        scope: Default::default(),
        steps: vec![CommandWorkflowStep {
            id: "step-1".to_owned(),
            title: "List".to_owned(),
            command: "ls".to_owned(),
            description: None,
            scope: None,
            requires_confirmation: step_requires_confirmation,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        }],
        sort_order: 0,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

/// 验证命令历史统一 CRLF 与 CR，避免同一命令按平台换行分裂成多条内容。
#[test]
fn command_history_normalizes_line_endings() {
    let command = NormalizedCommandHistory::new("  echo one\r\necho two\r ").unwrap();
    assert_eq!(command.command, "echo one\necho two");
}

/// 验证容器目标历史继续写出旧 wire value，保持 SQLite/MCP 与旧版本读取兼容。
#[test]
fn container_history_target_preserves_existing_wire_value() {
    assert_eq!(
        serde_json::to_string(&CommandHistoryTarget::Container).unwrap(),
        "\"dockerContainer\""
    );
}

/// 验证有效模板变量通过声明名引用，未声明名和残缺花括号均在保存前拒绝。
#[test]
fn snippet_requires_well_formed_declared_placeholders() {
    assert!(
        snippet(
            "docker logs {{container}}",
            vec![text_variable("container")]
        )
        .validate()
        .is_ok()
    );
    assert!(
        snippet("docker logs {{missing}}", vec![text_variable("container")])
            .validate()
            .is_err()
    );
    assert!(
        snippet("docker logs {{container", vec![text_variable("container")])
            .validate()
            .is_err()
    );
}

/// 验证工作流 Pending 步骤不能直接报告完成，必须先启动或完成明确确认。
#[test]
fn workflow_requires_running_state_before_step_completion() {
    let mut run = WorkflowRun::start("run-1", &workflow(false)).unwrap();
    assert_eq!(run.state(), WorkflowRunState::Ready);
    assert_eq!(
        run.complete_current_step(0),
        Err(WorkflowTransitionError::InvalidState)
    );
    run.begin_current_step().unwrap();
    run.complete_current_step(0).unwrap();
    assert_eq!(run.state(), WorkflowRunState::Completed);
    assert_eq!(run.results()[0].state, WorkflowStepState::Succeeded);
}

/// 验证空工作流不进入无法前进或完成的 Running 状态。
#[test]
fn workflow_start_rejects_empty_steps() {
    let mut empty = workflow(false);
    empty.steps.clear();
    assert_eq!(
        WorkflowRun::start("run-empty", &empty).err(),
        Some(WorkflowTransitionError::EmptyWorkflow)
    );
}

/// 验证确认状态只能确认当前步骤，完成后重复确认不会越过终态。
#[test]
fn workflow_confirmation_is_scoped_to_current_step() {
    let mut run = WorkflowRun::start("run-confirm", &workflow(true)).unwrap();
    run.await_confirmation().unwrap();
    assert_eq!(run.state(), WorkflowRunState::AwaitingConfirmation);
    run.confirm_current_step().unwrap();
    run.complete_current_step(0).unwrap();
    assert_eq!(
        run.confirm_current_step(),
        Err(WorkflowTransitionError::InvalidState)
    );
}
