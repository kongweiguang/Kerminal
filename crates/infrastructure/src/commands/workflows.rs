//! `workflows/*.toml` 的 v1 文件格式与领域模型映射。
//!
//! @author kongweiguang

use std::path::Path;

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    commands::{VersionedCommandDefinition, WorkflowRepository},
};
use kerminal_domain::commands::{
    CommandWorkflow, CommandWorkflowStep, WorkflowListRequest, WorkflowScope,
};
use kerminal_domain::preferences::ConfigRevision;
use serde::{Deserialize, Serialize};

use super::catalog::{FileCommandCatalogRepository, document_id};

const SCHEMA_VERSION: u32 = 1;

/// 工作流磁盘结构沿用 v1 snake_case schema，与界面内部的 camelCase DTO 分离。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkflowTomlDocument {
    schema_version: u32,
    id: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    scope: WorkflowScope,
    #[serde(default)]
    steps: Vec<WorkflowStepTomlDocument>,
    #[serde(default)]
    sort_order: i64,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
}

/// 工作流步骤单独保留身份和顺序，编辑标题或命令时不生成新步骤 ID。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkflowStepTomlDocument {
    id: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    command: String,
    #[serde(default)]
    scope: Option<WorkflowScope>,
    #[serde(default)]
    requires_confirmation: bool,
    #[serde(default)]
    sort_order: i64,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
}

impl FileCommandCatalogRepository {
    /// 在共享锁中解析并校验整个目录，损坏条目会显式阻止不完整列表。
    fn list_workflows(&self, request: WorkflowListRequest) -> AppResult<Vec<CommandWorkflow>> {
        let _lock = self.lock()?;
        let paths = self.list_paths("workflows")?;
        let mut workflows = paths
            .iter()
            .map(|path| {
                self.read_workflow(path)
                    .map(|item| item.map(|(workflow, _)| workflow))
            })
            .collect::<AppResult<Vec<_>>>()?
            .into_iter()
            .flatten()
            .filter(|workflow| workflow_matches(workflow, &request))
            .collect::<Vec<_>>();
        workflows.sort_by(|left, right| {
            left.sort_order
                .cmp(&right.sort_order)
                .then_with(|| left.title.to_lowercase().cmp(&right.title.to_lowercase()))
                .then_with(|| left.id.cmp(&right.id))
        });
        workflows.truncate(100);
        Ok(workflows)
    }

    /// 读取特定定义及原始内容 revision；文件名和文档 ID 不一致时拒绝改绑。
    fn get_workflow(
        &self,
        id: &str,
    ) -> AppResult<
        Option<(
            CommandWorkflow,
            kerminal_domain::preferences::ConfigRevision,
        )>,
    > {
        let _lock = self.lock()?;
        let path = self.document_path("workflows", id)?;
        self.read_workflow(&path)
    }

    /// 解析 v1 DTO 并复用领域 validator，异常文件保持原状且不生成部分成功结果。
    fn read_workflow(
        &self,
        path: &Path,
    ) -> AppResult<
        Option<(
            CommandWorkflow,
            kerminal_domain::preferences::ConfigRevision,
        )>,
    > {
        let Some((source, revision)) = self.read_document(path)? else {
            return Ok(None);
        };
        let document: WorkflowTomlDocument = toml::from_str(&source)
            .map_err(|source| AppError::infrastructure_with_source("工作流文件格式无效", source))?;
        if document.schema_version != SCHEMA_VERSION || document.id != document_id(path)? {
            return Err(AppError::conflict(
                "工作流版本或文件标识不匹配，原文件已保留",
            ));
        }
        let workflow = to_workflow(document);
        let inputs = workflow
            .steps
            .iter()
            .map(|step| kerminal_domain::commands::WorkflowStepInput {
                id: Some(step.id.clone()),
                title: step.title.clone(),
                command: step.command.clone(),
                description: step.description.clone(),
                scope: step.scope,
                requires_confirmation: step.requires_confirmation,
            })
            .collect::<Vec<_>>();
        kerminal_domain::commands::validate_workflow(
            &workflow.title,
            workflow.description.as_deref(),
            &workflow.tags,
            &inputs,
        )
        .map_err(|_| AppError::invalid("工作流定义未通过校验"))?;
        Ok(Some((workflow, revision)))
    }

    /// 新建工作流只占用不存在的 ID，避免旧的非版本化调用覆盖外部编辑。
    fn create_workflow(&self, workflow: CommandWorkflow) -> AppResult<CommandWorkflow> {
        validate_workflow_definition(&workflow)?;
        let _lock = self.lock()?;
        let path = self.document_path("workflows", &workflow.id)?;
        if self.read_document(&path)?.is_some() {
            return Err(AppError::conflict("工作流已存在，请先读取 revision 再更新"));
        }
        let document = WorkflowTomlDocument::new(workflow.clone(), None);
        let source = toml::to_string_pretty(&document)
            .map_err(|source| AppError::infrastructure_with_source("序列化工作流失败", source))?;
        self.write_document(&path, &source, None)?;
        Ok(workflow)
    }

    /// 按打开时 revision 原子保存工作流，更新时保留定义创建时间和整体排序。
    fn save_workflow_versioned(
        &self,
        workflow: CommandWorkflow,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<CommandWorkflow>> {
        self.ensure_writable()?;
        validate_workflow_definition(&workflow)?;
        let _lock = self.lock()?;
        let path = self.document_path("workflows", &workflow.id)?;
        let previous = self.read_workflow(&path)?;
        match (&expected_revision, previous.as_ref()) {
            (None, None) => {}
            (Some(expected), Some((_, actual))) if expected == actual => {}
            _ => {
                return Err(AppError::conflict(
                    "工作流已被其它操作修改，请重新读取后保存",
                ));
            }
        }
        let previous_document = if previous.is_some() {
            self.read_document(&path)?
                .map(|(source, _)| {
                    toml::from_str::<WorkflowTomlDocument>(&source).map_err(|source| {
                        AppError::infrastructure_with_source("工作流文件格式无效", source)
                    })
                })
                .transpose()?
        } else {
            None
        };
        let document = WorkflowTomlDocument::new(workflow.clone(), previous_document.as_ref());
        let source = toml::to_string_pretty(&document)
            .map_err(|source| AppError::infrastructure_with_source("序列化工作流失败", source))?;
        let revision = self.write_document(&path, &source, expected_revision.as_ref())?;
        Ok(VersionedCommandDefinition {
            definition: workflow,
            revision: Some(revision),
        })
    }

    /// 删除用户工作流，运行态由 application 聚合管理，不随定义文件删除而伪造终态。
    fn remove_workflow(&self, id: &str) -> AppResult<bool> {
        self.ensure_writable()?;
        let _lock = self.lock()?;
        let path = self.document_path("workflows", id)?;
        let revision = self.read_workflow(&path)?.map(|(_, revision)| revision);
        self.delete_document(&path, revision.as_ref())
    }

    /// 只在原工作流 revision 未变化时删除，避免旧 UI 对外部新内容执行删除。
    fn remove_workflow_versioned(
        &self,
        id: &str,
        expected_revision: &ConfigRevision,
    ) -> AppResult<bool> {
        self.ensure_writable()?;
        let _lock = self.lock()?;
        let path = self.document_path("workflows", id)?;
        let Some((_, current_revision)) = self.read_workflow(&path)? else {
            return Ok(false);
        };
        if &current_revision != expected_revision {
            return Err(AppError::conflict(
                "工作流已被其它操作修改，请重新读取后删除",
            ));
        }
        self.delete_document(&path, Some(expected_revision))
    }
}

#[async_trait]
impl WorkflowRepository for FileCommandCatalogRepository {
    /// 通过阻塞池枚举和解析用户工作流，保持 UI 和网络 runtime 不执行同步文件 I/O。
    async fn list(&self, request: WorkflowListRequest) -> AppResult<Vec<CommandWorkflow>> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || repository.list_workflows(request))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("工作流列表任务意外终止", source)
            })?
    }

    /// 读取单一稳定 ID，不以首条定义兜底失效目标。
    async fn get(&self, workflow_id: String) -> AppResult<Option<CommandWorkflow>> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || {
            repository
                .get_workflow(&workflow_id)
                .map(|item| item.map(|(workflow, _)| workflow))
        })
        .await
        .map_err(|source| AppError::infrastructure_with_source("工作流读取任务意外终止", source))?
    }

    /// 非版本化保存只创建新定义；现有工作流必须经 CAS 更新用例提交。
    async fn save(&self, workflow: CommandWorkflow) -> AppResult<CommandWorkflow> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || repository.create_workflow(workflow))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("工作流保存任务意外终止", source)
            })?
    }

    /// 按稳定 ID 删除指定定义并返回删除事实。
    async fn delete(&self, workflow_id: String) -> AppResult<bool> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || repository.remove_workflow(&workflow_id))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("工作流删除任务意外终止", source)
            })?
    }

    /// 返回解析后的工作流和原始内容 revision。
    async fn get_versioned(
        &self,
        workflow_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<CommandWorkflow>>> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || {
            repository.get_workflow(&workflow_id).map(|item| {
                item.map(|(definition, revision)| VersionedCommandDefinition {
                    definition,
                    revision: Some(revision),
                })
            })
        })
        .await
        .map_err(|source| AppError::infrastructure_with_source("工作流读取任务意外终止", source))?
    }

    /// 在阻塞线程内提交 CAS 文件更新，不让序列化或 fsync 卡住异步运行时。
    async fn save_versioned(
        &self,
        workflow: CommandWorkflow,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<CommandWorkflow>> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || {
            repository.save_workflow_versioned(workflow, expected_revision)
        })
        .await
        .map_err(|source| AppError::infrastructure_with_source("工作流保存任务意外终止", source))?
    }

    /// 删除操作在阻塞线程中复核 revision，保证其他命令配置操作不会同时改写文件。
    async fn delete_versioned(
        &self,
        workflow_id: String,
        expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || {
            repository.remove_workflow_versioned(&workflow_id, &expected_revision)
        })
        .await
        .map_err(|source| AppError::infrastructure_with_source("工作流删除任务意外终止", source))?
    }
}

impl WorkflowTomlDocument {
    /// 更新定义时复用原创建时间与排序，新步骤元数据由步骤 ID 决定是否复用。
    fn new(workflow: CommandWorkflow, previous: Option<&Self>) -> Self {
        let now = now_unix_millis();
        let steps = workflow
            .steps
            .into_iter()
            .enumerate()
            .map(|(index, step)| {
                let old = previous
                    .and_then(|document| document.steps.iter().find(|item| item.id == step.id));
                WorkflowStepTomlDocument {
                    id: step.id,
                    title: step.title,
                    description: step.description,
                    command: step.command,
                    scope: step.scope,
                    requires_confirmation: step.requires_confirmation,
                    sort_order: index as i64,
                    created_at: old.map_or_else(|| now.clone(), |item| item.created_at.clone()),
                    updated_at: now.clone(),
                }
            })
            .collect();
        Self {
            schema_version: SCHEMA_VERSION,
            id: workflow.id,
            title: workflow.title,
            description: workflow.description,
            tags: workflow.tags,
            scope: workflow.scope,
            steps,
            sort_order: previous.map_or(workflow.sort_order, |document| document.sort_order),
            created_at: previous
                .map_or_else(|| now.clone(), |document| document.created_at.clone()),
            updated_at: now,
        }
    }
}

/// 将文件 DTO 映射为领域定义，并依据显式 sort_order 排好步骤。
fn to_workflow(mut document: WorkflowTomlDocument) -> CommandWorkflow {
    document.steps.sort_by_key(|step| step.sort_order);
    CommandWorkflow {
        id: document.id,
        title: document.title,
        description: document.description,
        tags: document.tags,
        scope: document.scope,
        steps: document.steps.into_iter().map(to_step).collect(),
        sort_order: document.sort_order,
        created_at: document.created_at,
        updated_at: document.updated_at,
    }
}

/// 把一个磁盘步骤转换成独立领域值，保留可选 scope 和确认约束。
fn to_step(step: WorkflowStepTomlDocument) -> CommandWorkflowStep {
    CommandWorkflowStep {
        id: step.id,
        title: step.title,
        command: step.command,
        description: step.description,
        scope: step.scope,
        requires_confirmation: step.requires_confirmation,
        sort_order: step.sort_order,
        created_at: step.created_at,
        updated_at: step.updated_at,
    }
}

/// 只允许非空、字段长度有效且步骤完整的定义写入 TOML。
fn validate_workflow_definition(workflow: &CommandWorkflow) -> AppResult<()> {
    let mut ids = std::collections::HashSet::with_capacity(workflow.steps.len());
    if workflow.steps.iter().any(|step| {
        step.id.is_empty()
            || step.id.chars().count() > 160
            || step.id.chars().any(char::is_control)
            || !ids.insert(step.id.as_str())
    }) {
        return Err(AppError::invalid("工作流步骤标识无效或重复"));
    }
    let inputs = workflow
        .steps
        .iter()
        .map(|step| kerminal_domain::commands::WorkflowStepInput {
            id: Some(step.id.clone()),
            title: step.title.clone(),
            command: step.command.clone(),
            description: step.description.clone(),
            scope: step.scope,
            requires_confirmation: step.requires_confirmation,
        })
        .collect::<Vec<_>>();
    kerminal_domain::commands::validate_workflow(
        &workflow.title,
        workflow.description.as_deref(),
        &workflow.tags,
        &inputs,
    )
    .map_err(|_| AppError::invalid("工作流定义未通过校验"))
}

/// 将关键词、scope、tag 组合为不区分大小写的稳定定义筛选。
fn workflow_matches(workflow: &CommandWorkflow, request: &WorkflowListRequest) -> bool {
    if request.scope.is_some_and(|scope| workflow.scope != scope)
        || request.tag.as_deref().is_some_and(|tag| {
            !workflow
                .tags
                .iter()
                .any(|item| item.eq_ignore_ascii_case(tag))
        })
    {
        return false;
    }
    request.query.as_deref().is_none_or(|query| {
        let query = query.to_lowercase();
        workflow.title.to_lowercase().contains(&query)
            || workflow
                .description
                .as_deref()
                .is_some_and(|text| text.to_lowercase().contains(&query))
            || workflow
                .tags
                .iter()
                .any(|tag| tag.to_lowercase().contains(&query))
            || workflow.steps.iter().any(|step| {
                step.title.to_lowercase().contains(&query)
                    || step.command.to_lowercase().contains(&query)
            })
    })
}

/// 使用毫秒 epoch 维护文件审计字段，系统时钟异常时不 panic。
fn now_unix_millis() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}
