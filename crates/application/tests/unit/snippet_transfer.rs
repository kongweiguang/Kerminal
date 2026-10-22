//! 旧 JSON v1 片段交换契约的边界回归。
//!
//! @author kongweiguang

use async_trait::async_trait;
use kerminal_domain::commands::{
    SnippetCatalogItem, SnippetCatalogOrigin, SnippetContextBinding, SnippetDefaultAction,
    SnippetDefinition, SnippetRisk, SnippetScope, SnippetVariable, SnippetVariableKind,
    SnippetVariableRenderStrategy,
};
use kerminal_domain::preferences::ConfigRevision;
use serde_json::Value;
use std::sync::{Arc, Mutex};

use super::*;

const PREVIOUS_BATCH_ROUNDTRIP_ITEMS: usize = 501;

/// 预览忽略导出源 ID、生成新的用户身份，并保留重复标题为可选择警告。
#[test]
fn preview_generates_fresh_identity_and_marks_title_conflict() {
    let preview = preview_document(
        r#"{"schemaVersion":1,"exportedAt":"2026-10-17T00:00:00.000Z","snippets":[{"id":"old-id","title":" List ","command":"echo {{path}}","scope":"any","risk":"inspect","defaultAction":"insert","variables":[{"name":"path","label":"路径","description":"","kind":"path","required":true,"sensitive":false,"suggestions":[],"renderStrategy":"shellArg"}]}]}"#,
        &[catalog_item("List")],
    );

    assert_eq!(preview.candidates.len(), 1);
    assert!(preview.issues.is_empty());
    assert!(preview.candidates[0].title_conflicts);
    assert_ne!(preview.candidates[0].definition.id, "old-id");
    assert_eq!(
        preview.candidates[0].definition.category.as_deref(),
        Some("custom")
    );
    assert_eq!(
        preview.candidates[0].definition.description.as_deref(),
        Some("")
    );
}

/// 明文凭据命令与敏感变量预设不能进入预览候选，错误只显示固定类别。
#[test]
fn preview_rejects_sensitive_literals_and_secret_defaults() {
    let preview = preview_document(
        r#"{"schemaVersion":1,"snippets":[{"title":"unsafe","command":"echo password=abcd","scope":"any","risk":"inspect","defaultAction":"insert"},{"title":"secret","command":"echo {{token}}","scope":"any","risk":"inspect","defaultAction":"insert","variables":[{"name":"token","label":"Token","kind":"secret","required":false,"sensitive":true,"defaultValue":"do-not-export","suggestions":[],"renderStrategy":"shellArg"}]}]}"#,
        &[],
    );

    assert!(preview.candidates.is_empty());
    assert_eq!(preview.issues.len(), 2);
    assert!(
        preview
            .issues
            .iter()
            .all(|issue| !issue.message.contains("do-not-export"))
    );
}

/// 结构错误和超量输入都作为预览 issue 返回，不接受部分静默截断的批次。
/// @author kongweiguang
#[test]
fn preview_rejects_unsupported_document_and_oversized_batch() {
    assert_eq!(preview_document("{}", &[]).issues.len(), 1);
    let rows = vec![Value::Null; MAX_SNIPPET_TRANSFER_ITEMS + 1];
    let oversized = serde_json::json!({"schemaVersion": 1, "snippets": rows});
    let preview = preview_document(&oversized.to_string(), &[]);
    assert!(preview.candidates.is_empty());
    assert_eq!(preview.issues[0].source_index, None);
}

/// JSON v1 导出省略敏感默认值与候选建议，不携带片段身份或使用统计。
/// @author kongweiguang
#[test]
fn export_scrubs_sensitive_variable_values_and_usage_metadata() {
    let item = catalog_item("Sensitive");
    let mut item = item;
    item.variables = vec![
        SnippetVariable {
            name: "token".to_owned(),
            label: "Token".to_owned(),
            description: String::new(),
            kind: SnippetVariableKind::Secret,
            required: false,
            default_value: Some("do-not-export".to_owned()),
            suggestions: vec!["must-not-export".to_owned()],
            validation: None,
            render_strategy: SnippetVariableRenderStrategy::ShellArg,
            sensitive: true,
        },
        SnippetVariable {
            name: "path".to_owned(),
            label: "Path".to_owned(),
            description: String::new(),
            kind: SnippetVariableKind::Path,
            required: false,
            default_value: None,
            suggestions: Vec::new(),
            validation: None,
            render_strategy: SnippetVariableRenderStrategy::ShellArg,
            sensitive: false,
        },
    ];
    item.use_count = 10;
    item.favorite = true;

    let exported = export_candidate(&item);
    let json = serde_json::to_value(exported).expect("candidate serializes");
    let json = serde_json::json!({
        "schemaVersion": 1,
        "exportedAt": utc_timestamp(),
        "snippets": [json]
    });

    assert!(json.pointer("/snippets/0/id").is_none());
    assert!(json.pointer("/snippets/0/favorite").is_none());
    assert!(json.pointer("/snippets/0/useCount").is_none());
    assert!(
        json.pointer("/snippets/0/variables/0/defaultValue")
            .is_none()
    );
    assert_eq!(
        json.pointer("/snippets/0/variables/1/defaultValue"),
        Some(&Value::Null)
    );
    assert_eq!(
        json["snippets"][0]["variables"][0]["suggestions"],
        serde_json::json!([])
    );
    let serialized = serde_json::to_string(&json).expect("导出 JSON 可序列化");
    assert!(!serialized.contains("do-not-export"));
    assert!(!serialized.contains("must-not-export"));
}

/// 确认旧版 501 条清单可完整导出并参与重名检查，避免收紧传输预算时丢掉兼容范围。
/// @author kongweiguang
#[tokio::test]
async fn transfer_reads_complete_catalog_beyond_previous_batch_limit() {
    let items = (0..PREVIOUS_BATCH_ROUNDTRIP_ITEMS)
        .map(|index| {
            let title = if index == PREVIOUS_BATCH_ROUNDTRIP_ITEMS - 1 {
                "  SnIpPeT\t  500  ".to_owned()
            } else {
                format!("Snippet {index}")
            };
            let mut item = catalog_item(&title);
            item.id = format!("snippet-{index}");
            item
        })
        .collect::<Vec<_>>();
    let last_request = Arc::new(Mutex::new(None));
    let repository = AllUserSnippets {
        items,
        last_request: last_request.clone(),
    };

    let exported = export_user_snippets_json(&repository)
        .await
        .expect("完整目录应可导出");
    let exported: Value = serde_json::from_str(&exported).expect("导出仍为 JSON v1");
    assert_eq!(
        exported["snippets"].as_array().map(Vec::len),
        Some(PREVIOUS_BATCH_ROUNDTRIP_ITEMS)
    );
    assert_eq!(
        exported["snippets"][PREVIOUS_BATCH_ROUNDTRIP_ITEMS - 1]["title"],
        "  SnIpPeT\t  500  "
    );
    assert_eq!(
        last_request
            .lock()
            .expect("测试请求锁可用")
            .as_ref()
            .and_then(|request| request.limit),
        None
    );

    let existing = list_all_user_snippets(&repository)
        .await
        .expect("完整目录可用于冲突检查");
    let source = serde_json::json!({
        "schemaVersion": 1,
        "snippets": [{
            "title": "snippet 500",
            "command": "echo imported",
            "scope": "any",
            "risk": "inspect",
            "defaultAction": "insert"
        }]
    });
    let preview = preview_document(&source.to_string(), &existing);
    assert!(preview.candidates[0].title_conflicts);
}

/// 501 条旧版清单在统一预算下仍可作为一个批次完成预览和应用层校验。
/// @author kongweiguang
#[test]
fn previous_501_item_export_remains_one_valid_import_batch() {
    let rows = (0..PREVIOUS_BATCH_ROUNDTRIP_ITEMS)
        .map(|index| {
            serde_json::json!({
                "title": format!("Snippet {index}"),
                "command": "echo safe",
                "scope": "any",
                "risk": "inspect",
                "defaultAction": "insert"
            })
        })
        .collect::<Vec<_>>();
    let document = serde_json::json!({"schemaVersion": 1, "snippets": rows});
    let preview = preview_document(&document.to_string(), &[]);
    assert_eq!(preview.candidates.len(), PREVIOUS_BATCH_ROUNDTRIP_ITEMS);
    assert!(preview.issues.is_empty());

    let request = SnippetImportBatchRequest {
        batch_id: "legacy-501-items".to_owned(),
        definitions: preview
            .candidates
            .into_iter()
            .map(|candidate| candidate.definition)
            .collect(),
    };
    validate_batch_request(&request).expect("旧 501 条清单仍保持单批导入");
}

/// 超出共享条目预算时整份导出失败，不返回不完整但看似成功的 JSON。
/// @author kongweiguang
#[tokio::test]
async fn export_rejects_more_than_one_thousand_items_without_truncation() {
    let items = (0..=MAX_SNIPPET_TRANSFER_ITEMS)
        .map(|index| {
            let mut item = catalog_item(&format!("Snippet {index}"));
            item.id = format!("snippet-{index}");
            item
        })
        .collect();
    let repository = AllUserSnippets {
        items,
        last_request: Arc::new(Mutex::new(None)),
    };

    assert!(export_user_snippets_json(&repository).await.is_err());
}

/// 序列化逐块执行 32 MiB 实际输出预算，超限时不产生可保存的部分文件。
/// @author kongweiguang
#[tokio::test]
async fn export_rejects_serialized_json_over_byte_budget() {
    let mut item = catalog_item("Large description");
    item.description = "x".repeat(MAX_SNIPPET_TRANSFER_BYTES + 1);
    let repository = AllUserSnippets {
        items: vec![item],
        last_request: Arc::new(Mutex::new(None)),
    };

    assert!(export_user_snippets_json(&repository).await.is_err());
}

/// 1,000 条、最长合法 ID 和含引号/反斜线的路径必须完整导出、预览并通过批次校验。
/// @author kongweiguang
#[tokio::test]
async fn maximum_item_batch_round_trips_escaped_paths_and_long_ids() {
    let items = (0..MAX_SNIPPET_TRANSFER_ITEMS)
        .map(|index| {
            let mut item = catalog_item(&format!("路径片段 {index}"));
            item.id = format!("{:0>160}", index);
            item.template = format!("printf '%s' \"C:\\\\temp\\\\程序 \"{index}\"\"");
            item.description = format!("路径：C:\\临时\\\"目录\"\\item-{index}");
            item
        })
        .collect();
    let repository = AllUserSnippets {
        items,
        last_request: Arc::new(Mutex::new(None)),
    };

    let exported = export_user_snippets_json(&repository)
        .await
        .expect("共享条目上限内的导出应完整成功");
    assert!(exported.len() <= MAX_SNIPPET_TRANSFER_BYTES);
    let exported: Value = serde_json::from_str(&exported).expect("JSON v1 可解析");
    assert_eq!(
        exported["snippets"].as_array().map(Vec::len),
        Some(MAX_SNIPPET_TRANSFER_ITEMS)
    );

    let source = exported.to_string();
    let preview = preview_document(&source, &[]);
    assert_eq!(preview.candidates.len(), MAX_SNIPPET_TRANSFER_ITEMS);
    assert!(preview.issues.is_empty());
    let definitions = preview
        .candidates
        .into_iter()
        .enumerate()
        .map(|(index, mut candidate)| {
            candidate.definition.id = format!("{:0>160}", index);
            candidate.definition
        })
        .collect::<Vec<_>>();
    assert!(
        definitions
            .iter()
            .all(|definition| definition.id.len() == 160)
    );
    let request = SnippetImportBatchRequest {
        batch_id: "max-item-roundtrip".to_owned(),
        definitions,
    };
    validate_batch_request(&request).expect("最长合法 ID 和完整批次应通过边界校验");
}

/// 直接提交 1,001 个有效定义也必须整批拒绝，不能依赖 UI 预览限制。
/// @author kongweiguang
#[test]
fn import_commit_rejects_one_item_over_the_shared_limit() {
    let definitions = (0..=MAX_SNIPPET_TRANSFER_ITEMS)
        .map(|index| transfer_definition(format!("{:0>160}", index), index))
        .collect();
    let request = SnippetImportBatchRequest {
        batch_id: "over-item-limit".to_owned(),
        definitions,
    };

    assert!(validate_batch_request(&request).is_err());
}

struct AllUserSnippets {
    items: Vec<SnippetCatalogItem>,
    last_request: Arc<Mutex<Option<SnippetCatalogListRequest>>>,
}

#[async_trait]
impl SnippetRepository for AllUserSnippets {
    /// Fake 仓储按现有 port 语义将无 limit 请求完整返回，并记录 transfer 的真实请求。
    async fn list(&self, request: SnippetCatalogListRequest) -> AppResult<Vec<SnippetCatalogItem>> {
        *self
            .last_request
            .lock()
            .map_err(|_| AppError::infrastructure("测试目录请求锁中毒"))? = Some(request.clone());
        let mut items = self
            .items
            .iter()
            .filter(|item| request.origin.is_none_or(|origin| origin == item.origin))
            .cloned()
            .collect::<Vec<_>>();
        if let Some(limit) = request.limit {
            items.truncate(limit);
        }
        Ok(items)
    }

    /// 该 transfer 回归只读取目录，不执行收藏身份检查。
    async fn catalog_identity_exists(
        &self,
        _origin: SnippetCatalogOrigin,
        _snippet_id: String,
    ) -> AppResult<bool> {
        Err(AppError::unsupported("test endpoint not used"))
    }

    /// 该 transfer 回归不提交导入文件。
    async fn commit_import_batch(
        &self,
        _request: SnippetImportBatchRequest,
    ) -> AppResult<SnippetImportBatchState> {
        Err(AppError::unsupported("test endpoint not used"))
    }

    /// 该 transfer 回归不查询 journal 状态。
    async fn import_batch_status(&self, _batch_id: String) -> AppResult<SnippetImportBatchState> {
        Err(AppError::unsupported("test endpoint not used"))
    }

    /// 该 transfer 回归不读取可编辑定义。
    async fn get_user(&self, _snippet_id: String) -> AppResult<Option<SnippetDefinition>> {
        Err(AppError::unsupported("test endpoint not used"))
    }

    /// 该 transfer 回归不保存定义。
    async fn save_user(&self, _definition: SnippetDefinition) -> AppResult<SnippetDefinition> {
        Err(AppError::unsupported("test endpoint not used"))
    }

    /// 该 transfer 回归不删除定义。
    async fn delete_user(&self, _snippet_id: String) -> AppResult<bool> {
        Err(AppError::unsupported("test endpoint not used"))
    }

    /// 该 transfer 回归不读取版本化定义。
    async fn get_user_versioned(
        &self,
        _snippet_id: String,
    ) -> AppResult<Option<crate::commands::VersionedCommandDefinition<SnippetDefinition>>> {
        Err(AppError::unsupported("test endpoint not used"))
    }

    /// 该 transfer 回归不保存版本化定义。
    async fn save_user_versioned(
        &self,
        _definition: SnippetDefinition,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<crate::commands::VersionedCommandDefinition<SnippetDefinition>> {
        Err(AppError::unsupported("test endpoint not used"))
    }

    /// 该 transfer 回归不删除版本化定义。
    async fn delete_user_versioned(
        &self,
        _snippet_id: String,
        _expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        Err(AppError::unsupported("test endpoint not used"))
    }
}

/// 为纯 transfer 测试构造最小目录投影，避免使用数据库或文件 fixture。
fn catalog_item(title: &str) -> SnippetCatalogItem {
    SnippetCatalogItem {
        id: "stored-id".to_owned(),
        origin: SnippetCatalogOrigin::User,
        title: title.to_owned(),
        description: "description".to_owned(),
        template: "echo safe".to_owned(),
        category: "custom".to_owned(),
        pack: "mine".to_owned(),
        tags: Vec::new(),
        scope: SnippetScope::Any,
        platforms: Vec::new(),
        shells: Vec::new(),
        capabilities: Vec::new(),
        risk: SnippetRisk::Inspect,
        sensitive: false,
        duration: String::new(),
        default_action: SnippetDefaultAction::Insert,
        variables: Vec::new(),
        context_bindings: Vec::<SnippetContextBinding>::new(),
        derived_from: Some("snippet.builtin.base".to_owned()),
        catalog_version: None,
        command_spec: None,
        owner: None,
        tested_version: None,
        source_name: None,
        source_url: None,
        deprecated: false,
        favorite: false,
        use_count: 0,
        last_used_at_unix_ms: None,
        sort_order: 0,
        updated_at: String::new(),
    }
}

/// 构造通过真实领域定义校验的导入 DTO，测试提交端口而不绕过变量与模板规则。
/// @author kongweiguang
fn transfer_definition(id: String, index: usize) -> SnippetDefinition {
    SnippetDefinition {
        id,
        title: format!("路径片段 {index}"),
        command: format!("printf '%s' \"C:\\\\temp\\\\程序 {index}\""),
        description: Some(format!("路径：C:\\临时\\\"目录\"\\item-{index}")),
        tags: vec!["windows".to_owned()],
        sort_order: index as i64,
        scope: SnippetScope::Any,
        category: Some("custom".to_owned()),
        risk: SnippetRisk::Inspect,
        default_action: SnippetDefaultAction::Insert,
        variables: Vec::new(),
        context_bindings: Vec::new(),
        derived_from: None,
    }
}
