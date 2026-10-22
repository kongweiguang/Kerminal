//! 文件化片段与工作流的 revision-aware 用例。
//!
//! @author kongweiguang

use kerminal_domain::{
    commands::{
        CommandWorkflow, SnippetDefinition, WorkflowStepInput, WorkflowUpdateRequest,
        validate_workflow,
    },
    preferences::ConfigRevision,
};

use super::{AppError, AppResult, CommandsService, apply_workflow_update, validate_catalog_id};

/// 文件化定义与原始内容 revision 绑定，外部编辑后旧界面草稿必须冲突。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionedCommandDefinition<T> {
    pub definition: T,
    pub revision: Option<ConfigRevision>,
}

impl CommandsService {
    /// 打开片段时把原始内容 revision 一并交给界面，用于后续 CAS 写入。
    pub async fn get_user_snippet_versioned(
        &self,
        snippet_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<SnippetDefinition>>> {
        validate_catalog_id(&snippet_id)?;
        self.snippets.get_user_versioned(snippet_id).await
    }

    /// 按打开时 revision 保存片段，外部编辑已变更文件时保留新文件并提示重载。
    pub async fn save_snippet_versioned(
        &self,
        definition: SnippetDefinition,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<SnippetDefinition>> {
        validate_catalog_id(&definition.id)?;
        definition
            .validate()
            .map_err(|_| AppError::invalid("命令片段定义未通过校验"))?;
        self.snippets
            .save_user_versioned(definition, expected_revision)
            .await
    }

    /// 使用打开时的 revision 删除片段，陈旧界面不会移除编辑器刚更新的文件。
    pub async fn delete_snippet_versioned(
        &self,
        snippet_id: String,
        expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        validate_catalog_id(&snippet_id)?;
        self.snippets
            .delete_user_versioned(snippet_id, expected_revision)
            .await
    }

    /// 编辑工作流时返回文件 revision，保存时可检测 TOML 编辑器的并发更新。
    pub async fn get_workflow_versioned(
        &self,
        workflow_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<CommandWorkflow>>> {
        validate_catalog_id(&workflow_id)?;
        self.workflows.get_versioned(workflow_id).await
    }

    /// 校验完整定义后按期望 revision 创建或替换工作流文件。
    pub async fn save_workflow_versioned(
        &self,
        workflow: CommandWorkflow,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<CommandWorkflow>> {
        validate_catalog_id(&workflow.id)?;
        validate_workflow_definition(&workflow)?;
        self.workflows
            .save_versioned(workflow, expected_revision)
            .await
    }

    /// 更新工作流时验证步骤仍属于打开时的定义，并在写入时再次执行内容 CAS。
    pub async fn update_workflow_versioned(
        &self,
        request: WorkflowUpdateRequest,
        expected_revision: ConfigRevision,
    ) -> AppResult<VersionedCommandDefinition<CommandWorkflow>> {
        validate_catalog_id(&request.id)?;
        validate_workflow(
            &request.title,
            request.description.as_deref(),
            &request.tags,
            &request.steps,
        )
        .map_err(|_| AppError::invalid("工作流定义未通过校验"))?;
        let current = self
            .workflows
            .get_versioned(request.id.clone())
            .await?
            .ok_or_else(|| AppError::not_found("工作流不存在"))?;
        if current.revision.as_ref() != Some(&expected_revision) {
            return Err(AppError::conflict(
                "工作流已被其它操作修改，请重新读取后保存",
            ));
        }
        if request.steps.iter().any(|input| {
            input
                .id
                .as_deref()
                .is_some_and(|id| !current.definition.steps.iter().any(|step| step.id == id))
        }) {
            return Err(AppError::invalid("工作流步骤标识不属于当前定义"));
        }
        let workflow = apply_workflow_update(current.definition, request)?;
        self.workflows
            .save_versioned(workflow, Some(expected_revision))
            .await
    }

    /// 只有具备同一内容 revision 的 UI 草稿可以删除现有工作流。
    pub async fn delete_workflow_versioned(
        &self,
        workflow_id: String,
        expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        validate_catalog_id(&workflow_id)?;
        self.workflows
            .delete_versioned(workflow_id, expected_revision)
            .await
    }
}

/// 将同一领域工作流校验用于有版本保存入口，避免 UI 与旧保存入口形成不同规则。
fn validate_workflow_definition(workflow: &CommandWorkflow) -> AppResult<()> {
    let steps = workflow
        .steps
        .iter()
        .map(|step| WorkflowStepInput {
            id: Some(step.id.clone()),
            title: step.title.clone(),
            command: step.command.clone(),
            description: step.description.clone(),
            scope: step.scope,
            requires_confirmation: step.requires_confirmation,
        })
        .collect::<Vec<_>>();
    validate_workflow(
        &workflow.title,
        workflow.description.as_deref(),
        &workflow.tags,
        &steps,
    )
    .map_err(|_| AppError::invalid("工作流定义未通过校验"))
}
