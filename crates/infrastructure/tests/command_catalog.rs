//! 命令片段和工作流文件仓储的兼容、冲突与恢复隔离测试。
//!
//! @author kongweiguang

use std::{fs, path::PathBuf};

use kerminal_application::{
    AppError,
    commands::{SnippetRepository, WorkflowRepository},
};
use kerminal_domain::commands::{
    CommandWorkflow, CommandWorkflowStep, SnippetContextBinding, SnippetContextBindingKind,
    SnippetDefaultAction, SnippetDefinition, SnippetRisk, SnippetScope, SnippetVariable,
    SnippetVariableKind, SnippetVariableRenderStrategy, WorkflowListRequest, WorkflowScope,
};
use kerminal_infrastructure::{
    commands::FileCommandCatalogRepository, storage::StorageCoordinator,
};

struct TemporaryConfigRoot(PathBuf);

impl TemporaryConfigRoot {
    /// 每项契约测试使用独立根目录，避免触碰用户 `~/.kerminal` 或其他任务数据。
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("kerminal-command-catalog-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).expect("创建隔离配置目录");
        Self(root)
    }

    /// 构造使用同一存储协调器和指定启动写权限的测试仓储。
    fn repository(&self, writable: bool) -> FileCommandCatalogRepository {
        FileCommandCatalogRepository::new(StorageCoordinator::new(self.0.clone()), writable)
    }
}

impl Drop for TemporaryConfigRoot {
    /// 仅删除本测试创建的唯一临时根，保留其他测试和用户配置。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 新增和更新必须携带正确 revision，旧草稿不能覆盖最新内容。
#[tokio::test]
async fn snippet_save_uses_revision_compare_and_swap() {
    let temporary = TemporaryConfigRoot::new();
    let repository = temporary.repository(true);
    let created = repository
        .save_user_versioned(snippet("original"), None)
        .await
        .expect("create snippet");
    let revision = created.revision.expect("filesystem revision");

    let mut updated_definition = created.definition.clone();
    updated_definition.title = "最新标题".to_owned();
    let updated = repository
        .save_user_versioned(updated_definition, Some(revision.clone()))
        .await
        .expect("update snippet from matching revision");

    let mut stale_definition = created.definition;
    stale_definition.title = "过期标题".to_owned();
    assert!(matches!(
        repository
            .save_user_versioned(stale_definition, Some(revision))
            .await,
        Err(AppError::Conflict { .. })
    ));
    assert_eq!(
        repository
            .get_user_versioned("inspect-log".to_owned())
            .await
            .expect("read latest snippet")
            .expect("snippet exists")
            .definition
            .title,
        updated.definition.title
    );
}

/// 未来 schema 或损坏 TOML 返回错误并保留原始字节，不以默认值修复覆盖。
#[tokio::test]
async fn invalid_snippet_file_is_preserved() {
    let temporary = TemporaryConfigRoot::new();
    let directory = temporary.0.join("snippets");
    fs::create_dir_all(&directory).expect("create snippets directory");
    let path = directory.join("inspect-log.toml");
    let source = "schema_version = 88\nid = \"inspect-log\"\n";
    fs::write(&path, source).expect("write future-schema fixture");
    let repository = temporary.repository(true);

    assert!(repository.get_user("inspect-log".to_owned()).await.is_err());
    assert_eq!(
        fs::read_to_string(path).expect("read preserved fixture"),
        source
    );
}

/// 当前格式样本显式声明变量渲染策略；编辑只能改自有字段，不能丢失扩展键或用户注释。
#[tokio::test]
async fn snippet_edit_preserves_unknown_fields_and_comments() {
    let temporary = TemporaryConfigRoot::new();
    let directory = temporary.0.join("snippets");
    fs::create_dir_all(&directory).expect("create snippets directory");
    let path = directory.join("inspect-log.toml");
    let source = r#"# Keep this user note.
schema_version = 1
id = "inspect-log"
title = "Before" # preserve this inline comment
command = "echo {{name}}"
custom_top_level = "keep-top"

[[variables]]
name = "name"
label = "Name" # preserve variable comment
kind = "text"
required = true
render_strategy = "shellArg"
custom_variable_option = "keep-nested"

[[context_bindings]]
kind = "global"
custom_binding_option = "keep-binding"
"#;
    fs::write(&path, source).expect("write forward-compatible fixture");
    let repository = temporary.repository(true);
    let mut saved = repository
        .get_user_versioned("inspect-log".to_owned())
        .await
        .expect("read snippet")
        .expect("snippet exists");
    saved.definition.title = "After".to_owned();
    saved.definition.variables[0].label = "Display name".to_owned();

    repository
        .save_user_versioned(saved.definition, saved.revision)
        .await
        .expect("save edited snippet");
    let persisted = fs::read_to_string(path).expect("read updated file");
    assert!(persisted.contains("custom_top_level = \"keep-top\""));
    assert!(persisted.contains("custom_variable_option = \"keep-nested\""));
    assert!(persisted.contains("custom_binding_option = \"keep-binding\""));
    assert!(persisted.contains("# Keep this user note."));
    assert!(persisted.contains("title = \"After\" # preserve this inline comment"));
    assert!(persisted.contains("label = \"Display name\" # preserve variable comment"));
}

/// r20 路径变量与保存时补入的全局绑定需在重复 TOML 补丁后仍可读，非法删变量不可落盘。
#[tokio::test]
async fn snippet_variable_and_binding_arrays_survive_repeated_saves() {
    let temporary = TemporaryConfigRoot::new();
    let directory = temporary.0.join("snippets");
    fs::create_dir_all(&directory).expect("create snippets directory");
    let path = directory.join("r20-public-path-inspection.toml");
    let source = r#"# @author kongweiguang
# 公开验收样例；展示模板编辑和变量展开，不写凭据，也不会自动运行。
schema_version = 1
id = "r20-public-path-inspection"
title = "查看指定路径"
command = 'Get-ChildItem -LiteralPath {{path}}'
description = "公开验收片段。复制仅写入系统剪贴板，运行需明确操作。"
tags = ["公开示例", "文件"]
scope = "local"
sort_order = 100
created_at = "2026-10-17T00:00:00Z"
updated_at = "2026-10-17T00:00:00Z"
risk = "inspect"
default_action = "insert"

[[variables]]
name = "path"
label = "路径"
description = "要检查的本地路径"
kind = "path"
required = true
suggestions = []
render_strategy = "shellArg"
sensitive = false
"#;
    fs::write(&path, source).expect("write exact path variable fixture");
    let repository = temporary.repository(true);
    let expected_variable = SnippetVariable {
        name: "path".to_owned(),
        label: "路径".to_owned(),
        description: "要检查的本地路径".to_owned(),
        kind: SnippetVariableKind::Path,
        required: true,
        default_value: None,
        suggestions: Vec::new(),
        validation: None,
        render_strategy: SnippetVariableRenderStrategy::ShellArg,
        sensitive: false,
    };

    let mut opened = repository
        .get_user_versioned("r20-public-path-inspection".to_owned())
        .await
        .expect("read seeded path snippet")
        .expect("seeded snippet exists");
    assert_eq!(opened.definition.variables, vec![expected_variable.clone()]);
    opened.definition.context_bindings = vec![SnippetContextBinding {
        kind: SnippetContextBindingKind::Global,
        target_id: None,
    }];

    let first_save = repository
        .save_user_versioned(opened.definition, opened.revision)
        .await
        .expect("save path snippet with global binding");
    let first_source = fs::read_to_string(&path).expect("read first saved snippet");
    assert!(first_source.contains("[[variables]]"));
    assert!(first_source.contains("[[context_bindings]]"));
    assert_eq!(
        first_save.definition.variables,
        vec![expected_variable.clone()]
    );
    assert_eq!(
        first_save.definition.context_bindings,
        vec![SnippetContextBinding {
            kind: SnippetContextBindingKind::Global,
            target_id: None,
        }]
    );

    let mut second_definition = first_save.definition.clone();
    second_definition.title = "再次查看指定路径".to_owned();
    second_definition.variables[0].description = "要检查的本地目录".to_owned();
    let second_save = repository
        .save_user_versioned(second_definition, first_save.revision.clone())
        .await
        .expect("save path snippet a second time");
    let second_source = fs::read_to_string(&path).expect("read second saved snippet");
    assert!(second_source.contains("[[variables]]"));
    assert!(second_source.contains("[[context_bindings]]"));
    let reread = repository
        .get_user_versioned("r20-public-path-inspection".to_owned())
        .await
        .expect("reread second saved snippet")
        .expect("snippet remains readable");
    assert_eq!(reread.definition.variables[0].name, "path");
    assert_eq!(reread.definition.variables[0].label, "路径");
    assert_eq!(
        reread.definition.variables[0].description,
        "要检查的本地目录"
    );
    assert_eq!(
        reread.definition.context_bindings,
        vec![SnippetContextBinding {
            kind: SnippetContextBindingKind::Global,
            target_id: None,
        }]
    );

    let mut invalid_definition = second_save.definition;
    invalid_definition.variables.clear();
    assert!(matches!(
        repository
            .save_user_versioned(invalid_definition, second_save.revision)
            .await,
        Err(AppError::Invalid { .. })
    ));
    assert_eq!(
        fs::read_to_string(&path).expect("read unchanged valid snippet"),
        second_source
    );
}

/// 只读恢复快照拒绝命令目录写入，避免在坏配置状态下静默建立平行内容。
#[tokio::test]
async fn read_only_recovery_rejects_catalog_writes() {
    let temporary = TemporaryConfigRoot::new();
    let repository = temporary.repository(false);

    assert!(matches!(
        repository
            .save_user_versioned(snippet("inspect"), None)
            .await,
        Err(AppError::Conflict { .. })
    ));
    assert!(!temporary.0.join("snippets").exists());
}

/// 工作流步骤顺序、身份和配置内容经过文件化 round trip 后保持一致。
#[tokio::test]
async fn workflow_round_trip_preserves_step_identity_and_order() {
    let temporary = TemporaryConfigRoot::new();
    let repository = temporary.repository(true);
    let created = repository
        .save_versioned(workflow(), None)
        .await
        .expect("create workflow");
    let expected_revision = created.revision.expect("filesystem revision");
    let mut edited = created.definition;
    edited.steps.reverse();
    for (index, step) in edited.steps.iter_mut().enumerate() {
        step.sort_order = index as i64;
    }
    edited.steps[0].title = "读取状态".to_owned();
    let updated = repository
        .save_versioned(edited, Some(expected_revision))
        .await
        .expect("update workflow");

    let listed = WorkflowRepository::list(&repository, WorkflowListRequest::default())
        .await
        .expect("list workflows");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].steps[0].id, "inspect-b");
    assert_eq!(listed[0].steps[0].title, "读取状态");
    assert_eq!(listed[0].steps[1].id, "inspect-a");
    assert_eq!(
        updated.revision,
        repository
            .get_versioned("inspect-workflow".to_owned())
            .await
            .expect("read workflow")
            .expect("workflow exists")
            .revision
    );
}

/// 构造不含持久化敏感值的用户命令片段 fixture。
fn snippet(title: &str) -> SnippetDefinition {
    SnippetDefinition {
        id: "inspect-log".to_owned(),
        title: title.to_owned(),
        command: "tail -n 100 app.log".to_owned(),
        description: Some("只读日志检查".to_owned()),
        tags: vec!["日志".to_owned()],
        sort_order: 0,
        scope: SnippetScope::Ssh,
        category: Some("诊断".to_owned()),
        risk: SnippetRisk::Inspect,
        default_action: SnippetDefaultAction::Insert,
        variables: Vec::new(),
        context_bindings: Vec::new(),
        derived_from: None,
    }
}

/// 构造包含稳定 step ID 的本地工作流，便于验证持久化顺序不依赖 TOML 行序。
fn workflow() -> CommandWorkflow {
    CommandWorkflow {
        id: "inspect-workflow".to_owned(),
        title: "检查服务".to_owned(),
        description: Some("只读检查服务状态".to_owned()),
        tags: vec!["诊断".to_owned()],
        scope: WorkflowScope::Local,
        steps: vec![
            workflow_step("inspect-a", "读取日志", "tail -n 20 app.log", 0),
            workflow_step("inspect-b", "检查状态", "systemctl status app", 1),
        ],
        sort_order: 0,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    }
}

/// 复用最小工作流步骤 fixture，测试只通过领域实体进入文件仓储。
fn workflow_step(id: &str, title: &str, command: &str, sort_order: i64) -> CommandWorkflowStep {
    CommandWorkflowStep {
        id: id.to_owned(),
        title: title.to_owned(),
        command: command.to_owned(),
        description: None,
        scope: None,
        requires_confirmation: false,
        sort_order,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    }
}
