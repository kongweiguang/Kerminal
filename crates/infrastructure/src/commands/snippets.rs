//! `snippets/*.toml` 的 v1 文件格式与领域模型映射。
//!
//! @author kongweiguang

use std::collections::BTreeMap;

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    commands::{MAX_SNIPPET_TRANSFER_ITEMS, SnippetRepository, VersionedCommandDefinition},
};
use kerminal_domain::commands::{
    SnippetCatalogItem, SnippetCatalogListRequest, SnippetCatalogOrigin, SnippetContextBinding,
    SnippetContextBindingKind, SnippetDefaultAction, SnippetDefinition, SnippetRisk, SnippetScope,
    SnippetVariable, SnippetVariableKind, SnippetVariableRenderStrategy,
};
use kerminal_domain::preferences::ConfigRevision;
use serde::{Deserialize, Serialize};

use super::{
    builtin_snippets::builtin_snippet_catalog,
    catalog::{FileCommandCatalogRepository, document_id},
};

const SCHEMA_VERSION: u32 = 1;

/// 旧 `.kerminal/snippets` 文件保留 snake_case 字段名和未识别扩展项。
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct SnippetTomlDocument {
    schema_version: u32,
    id: String,
    title: String,
    command: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    scope: SnippetScope,
    #[serde(default)]
    sort_order: i64,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    risk: Option<SnippetRisk>,
    #[serde(default)]
    default_action: Option<SnippetDefaultAction>,
    #[serde(default)]
    variables: Vec<SnippetVariableTomlEntry>,
    #[serde(default)]
    context_bindings: Vec<SnippetContextBindingTomlEntry>,
    #[serde(default)]
    derived_from: Option<String>,
    /// 旧文件编辑采用 patch-on-source；这里为相同语义保存未被当前版本拥有的字段。
    #[serde(flatten)]
    extra_fields: BTreeMap<String, toml::Value>,
}

/// 变量有独立的 TOML 边界 DTO，避免领域 camelCase serde 改写旧 snake_case 配置。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
struct SnippetVariableTomlEntry {
    name: String,
    label: String,
    #[serde(default)]
    description: String,
    kind: SnippetVariableKind,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    default_value: Option<String>,
    #[serde(default)]
    suggestions: Vec<String>,
    #[serde(default)]
    validation: Option<String>,
    render_strategy: SnippetVariableRenderStrategy,
    #[serde(default)]
    sensitive: bool,
    #[serde(flatten)]
    extra_fields: BTreeMap<String, toml::Value>,
}

/// 上下文绑定 DTO 保持旧文件的 snake_case，同时保留版本间增加的绑定属性。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
struct SnippetContextBindingTomlEntry {
    kind: SnippetContextBindingKind,
    #[serde(default)]
    target_id: Option<String>,
    #[serde(flatten)]
    extra_fields: BTreeMap<String, toml::Value>,
}

impl SnippetVariableTomlEntry {
    /// 用户未改名的变量沿用磁盘扩展属性；变量内容本身始终由领域 DTO 校验。
    fn from_domain(variable: SnippetVariable, previous: Option<&Self>) -> Self {
        Self {
            name: variable.name,
            label: variable.label,
            description: variable.description,
            kind: variable.kind,
            required: variable.required,
            default_value: variable.default_value,
            suggestions: variable.suggestions,
            validation: variable.validation,
            render_strategy: variable.render_strategy,
            sensitive: variable.sensitive,
            extra_fields: previous.map_or_else(BTreeMap::new, |old| old.extra_fields.clone()),
        }
    }

    /// 只把已知字段映射进领域层；扩展字段继续留在文件仓储边界。
    fn into_domain(self) -> SnippetVariable {
        SnippetVariable {
            name: self.name,
            label: self.label,
            description: self.description,
            kind: self.kind,
            required: self.required,
            default_value: self.default_value,
            suggestions: self.suggestions,
            validation: self.validation,
            render_strategy: self.render_strategy,
            sensitive: self.sensitive,
        }
    }
}

impl SnippetContextBindingTomlEntry {
    /// 绑定以 kind+target 作为稳定对应键，避免正文修改时丢失未知扩展字段。
    fn from_domain(binding: SnippetContextBinding, previous: Option<&Self>) -> Self {
        Self {
            kind: binding.kind,
            target_id: binding.target_id,
            extra_fields: previous.map_or_else(BTreeMap::new, |old| old.extra_fields.clone()),
        }
    }

    /// 文件扩展属性保持在 adapter 中，不扩散进 UI 或领域 API。
    fn into_domain(self) -> SnippetContextBinding {
        SnippetContextBinding {
            kind: self.kind,
            target_id: self.target_id,
        }
    }
}

impl FileCommandCatalogRepository {
    /// 合并冻结的内置目录和用户文件；仅访问用户目录时才取得磁盘锁，保证内置片段可独立浏览。
    fn list_snippets(
        &self,
        request: SnippetCatalogListRequest,
    ) -> AppResult<Vec<SnippetCatalogItem>> {
        let mut items = if request.origin == Some(SnippetCatalogOrigin::User) {
            Vec::new()
        } else {
            builtin_snippet_catalog()?
        };
        if request.origin != Some(SnippetCatalogOrigin::Builtin) {
            let _lock = self.lock()?;
            self.ensure_import_journal_resolved()?;
            let paths = self.list_paths("snippets")?;
            items.reserve(paths.len());
            for path in &paths {
                if let Some((definition, document, _)) = self.read_snippet(path)? {
                    items.push(to_catalog_item(definition, document));
                }
            }
        }
        let query = request
            .query
            .as_deref()
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);
        items.retain(|item| snippet_matches(item, &request, query.as_deref()));
        items.sort_by(|left, right| {
            pack_rank(&left.pack)
                .cmp(&pack_rank(&right.pack))
                .then_with(|| left.pack.cmp(&right.pack))
                .then_with(|| left.category.cmp(&right.category))
                .then_with(|| left.sort_order.cmp(&right.sort_order))
                .then_with(|| left.id.cmp(&right.id))
        });
        if let Some(limit) = request.limit {
            items.truncate(limit.clamp(1, MAX_SNIPPET_TRANSFER_ITEMS));
        }
        Ok(items)
    }

    /// 按来源直接读取一条目录投影，避免详情和执行准备受列表分页或 transfer 上限影响。
    fn get_catalog_item_sync(
        &self,
        origin: SnippetCatalogOrigin,
        snippet_id: &str,
    ) -> AppResult<Option<SnippetCatalogItem>> {
        match origin {
            SnippetCatalogOrigin::Builtin => Ok(builtin_snippet_catalog()?
                .into_iter()
                .find(|item| item.id == snippet_id)),
            SnippetCatalogOrigin::User => {
                let _lock = self.lock()?;
                self.ensure_import_journal_resolved()?;
                let path = self.document_path("snippets", snippet_id)?;
                Ok(self
                    .read_snippet(&path)?
                    .map(|(definition, document, _)| to_catalog_item(definition, document)))
            }
        }
    }

    /// 读取定义、保留元数据和原字节 revision，文件名与文档 ID 不一致时拒绝绑定。
    fn get_snippet(
        &self,
        id: &str,
    ) -> AppResult<
        Option<(
            SnippetDefinition,
            kerminal_domain::preferences::ConfigRevision,
        )>,
    > {
        let _lock = self.lock()?;
        self.ensure_import_journal_resolved()?;
        let path = self.document_path("snippets", id)?;
        self.read_snippet(&path)
            .map(|value| value.map(|(definition, _, revision)| (definition, revision)))
    }

    /// 通过 v1 DTO 解析后再跑领域校验，坏文件被保留并作为明确错误返回。
    pub(super) fn read_snippet(
        &self,
        path: &std::path::Path,
    ) -> AppResult<
        Option<(
            SnippetDefinition,
            SnippetTomlDocument,
            kerminal_domain::preferences::ConfigRevision,
        )>,
    > {
        let Some((source, revision)) = self.read_document(path)? else {
            return Ok(None);
        };
        let document: SnippetTomlDocument = toml::from_str(&source).map_err(|source| {
            AppError::infrastructure_with_source("命令片段文件格式无效", source)
        })?;
        if document.schema_version != SCHEMA_VERSION || document.id != document_id(path)? {
            return Err(AppError::conflict(
                "命令片段版本或文件标识不匹配，原文件已保留",
            ));
        }
        let definition = to_definition(&document);
        definition
            .validate()
            .map_err(|_| AppError::invalid("命令片段定义未通过校验"))?;
        Ok(Some((definition, document, revision)))
    }

    /// 新建用户片段仅允许占用空文件名，旧调用无法覆写外部编辑产生的内容。
    fn create_snippet(&self, definition: SnippetDefinition) -> AppResult<SnippetDefinition> {
        definition
            .validate()
            .map_err(|_| AppError::invalid("命令片段定义未通过校验"))?;
        let _lock = self.lock()?;
        self.ensure_import_journal_resolved()?;
        let path = self.document_path("snippets", &definition.id)?;
        if self.read_document(&path)?.is_some() {
            return Err(AppError::conflict(
                "命令片段已存在，请先读取 revision 再更新",
            ));
        }
        let document = SnippetTomlDocument::new(definition.clone(), None);
        let source = serialize_snippet_document(None, &document)?;
        self.write_document(&path, &source, None)?;
        Ok(definition)
    }

    /// 按外部读取的原文 revision 原子创建或更新，并复用原有用户排序与创建时间。
    fn save_snippet_versioned(
        &self,
        definition: SnippetDefinition,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<SnippetDefinition>> {
        self.ensure_writable()?;
        definition
            .validate()
            .map_err(|_| AppError::invalid("命令片段定义未通过校验"))?;
        let _lock = self.lock()?;
        self.ensure_import_journal_resolved()?;
        let path = self.document_path("snippets", &definition.id)?;
        let previous = self.read_snippet(&path)?;
        match (&expected_revision, previous.as_ref()) {
            (None, None) => {}
            (Some(expected), Some((_, _, actual))) if expected == actual => {}
            _ => {
                return Err(AppError::conflict(
                    "命令片段已被其它操作修改，请重新读取后保存",
                ));
            }
        }
        let previous_source = match (previous.as_ref(), self.read_document(&path)?) {
            (None, None) => None,
            (Some((_, _, expected_revision)), Some((source, actual_revision)))
                if expected_revision == &actual_revision =>
            {
                Some(source)
            }
            _ => {
                return Err(AppError::conflict(
                    "命令片段在保存准备期间已被修改，请重新读取后保存",
                ));
            }
        };
        let document = SnippetTomlDocument::new(
            definition.clone(),
            previous.as_ref().map(|(_, document, _)| document),
        );
        let source = serialize_snippet_document(previous_source.as_deref(), &document)?;
        let revision = self.write_document(&path, &source, expected_revision.as_ref())?;
        Ok(VersionedCommandDefinition {
            definition,
            revision: Some(revision),
        })
    }

    /// 删除用户片段文件；内置片段不存在于此目录，因此无法被该仓储删除。
    fn remove_snippet(&self, id: &str) -> AppResult<bool> {
        self.ensure_writable()?;
        let _lock = self.lock()?;
        self.ensure_import_journal_resolved()?;
        let path = self.document_path("snippets", id)?;
        let revision = self.read_snippet(&path)?.map(|(_, _, revision)| revision);
        self.delete_document(&path, revision.as_ref())
    }

    /// 只有 revision 与当前文件内容相同时删除，文件变化时绝不以新内容覆盖旧用户意图。
    fn remove_snippet_versioned(
        &self,
        id: &str,
        expected_revision: &ConfigRevision,
    ) -> AppResult<bool> {
        self.ensure_writable()?;
        let _lock = self.lock()?;
        self.ensure_import_journal_resolved()?;
        let path = self.document_path("snippets", id)?;
        let Some((_, _, current_revision)) = self.read_snippet(&path)? else {
            return Ok(false);
        };
        if &current_revision != expected_revision {
            return Err(AppError::conflict(
                "命令片段已被其它操作修改，请重新读取后删除",
            ));
        }
        self.delete_document(&path, Some(expected_revision))
    }
}

#[async_trait]
impl SnippetRepository for FileCommandCatalogRepository {
    /// 通过阻塞池读取文件目录，GPUI 和 Tokio 网络任务不承担磁盘遍历。
    async fn list(&self, request: SnippetCatalogListRequest) -> AppResult<Vec<SnippetCatalogItem>> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || repository.list_snippets(request))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("命令片段列表任务意外终止", source)
            })?
    }

    /// 通过阻塞池直接解析 origin/id，防止超过目录上限或同 ID 另一来源影响目标选择。
    async fn get_catalog_item(
        &self,
        origin: SnippetCatalogOrigin,
        snippet_id: String,
    ) -> AppResult<Option<SnippetCatalogItem>> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || repository.get_catalog_item_sync(origin, &snippet_id))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("片段目录精确查询任务意外终止", source)
            })?
    }

    /// 将用户偏好身份绑定到真实目录项；内置来源从固定资源查验，用户来源走受锁文件读取。
    async fn catalog_identity_exists(
        &self,
        origin: SnippetCatalogOrigin,
        snippet_id: String,
    ) -> AppResult<bool> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || match origin {
            SnippetCatalogOrigin::Builtin => Ok(builtin_snippet_catalog()?
                .iter()
                .any(|item| item.id == snippet_id)),
            SnippetCatalogOrigin::User => repository
                .get_snippet(&snippet_id)
                .map(|snippet| snippet.is_some()),
        })
        .await
        .map_err(|source| {
            AppError::infrastructure_with_source("片段目录身份查询任务意外终止", source)
        })?
    }

    /// 在同一 repository implementation 中提交 import journal，避免第二份 trait adapter。
    async fn commit_import_batch(
        &self,
        request: kerminal_application::commands::SnippetImportBatchRequest,
    ) -> AppResult<kerminal_application::commands::SnippetImportBatchState> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || repository.commit_import_batch_sync(request))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("片段导入事务任务意外终止", source)
            })?
    }

    /// 查询导入状态只回读 receipt/journal，不会隐式再次运行文件创建步骤。
    async fn import_batch_status(
        &self,
        batch_id: String,
    ) -> AppResult<kerminal_application::commands::SnippetImportBatchState> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || repository.import_batch_status_sync(batch_id))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("片段导入状态查询任务意外终止", source)
            })?
    }

    /// 只读取用户定义；内置片段由独立只读目录提供，不伪造为可编辑文件。
    async fn get_user(&self, snippet_id: String) -> AppResult<Option<SnippetDefinition>> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || {
            repository
                .get_snippet(&snippet_id)
                .map(|item| item.map(|(definition, _)| definition))
        })
        .await
        .map_err(|source| {
            AppError::infrastructure_with_source("命令片段读取任务意外终止", source)
        })?
    }

    /// 新建只接受空 ID；更新需携带读取 revision 的版本化端口，避免覆盖外部修改。
    async fn save_user(&self, definition: SnippetDefinition) -> AppResult<SnippetDefinition> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || repository.create_snippet(definition))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("命令片段保存任务意外终止", source)
            })?
    }

    /// 在同一阻塞任务内确认目标仍为用户文件并执行受锁保护的删除。
    async fn delete_user(&self, snippet_id: String) -> AppResult<bool> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || repository.remove_snippet(&snippet_id))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("命令片段删除任务意外终止", source)
            })?
    }

    /// 返回可供编辑器保存的原始字节 revision。
    async fn get_user_versioned(
        &self,
        snippet_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<SnippetDefinition>>> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || {
            repository.get_snippet(&snippet_id).map(|item| {
                item.map(|(definition, revision)| VersionedCommandDefinition {
                    definition,
                    revision: Some(revision),
                })
            })
        })
        .await
        .map_err(|source| {
            AppError::infrastructure_with_source("命令片段读取任务意外终止", source)
        })?
    }

    /// 用共享文件锁和 CAS revision 原子保存定义，避免覆盖外部 TOML 编辑。
    async fn save_user_versioned(
        &self,
        definition: SnippetDefinition,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<SnippetDefinition>> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || {
            repository.save_snippet_versioned(definition, expected_revision)
        })
        .await
        .map_err(|source| {
            AppError::infrastructure_with_source("命令片段保存任务意外终止", source)
        })?
    }

    /// 对版本化删除请求再次检查文件 revision，防止关闭前被其它编辑器改写。
    async fn delete_user_versioned(
        &self,
        snippet_id: String,
        expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || {
            repository.remove_snippet_versioned(&snippet_id, &expected_revision)
        })
        .await
        .map_err(|source| {
            AppError::infrastructure_with_source("命令片段删除任务意外终止", source)
        })?
    }
}

impl SnippetTomlDocument {
    /// 仅向片段事务适配器暴露排序值，避免恢复/导入代码依赖完整文件 DTO。
    pub(super) fn sort_order(&self) -> i64 {
        self.sort_order
    }

    /// 保留既有排序和创建时间；新建时以毫秒时间戳形成稳定的文件元数据。
    fn new(definition: SnippetDefinition, previous: Option<&Self>) -> Self {
        let now = now_unix_millis();
        let variables = definition
            .variables
            .into_iter()
            .map(|variable| {
                let prior = previous.and_then(|document| {
                    document
                        .variables
                        .iter()
                        .find(|entry| entry.name == variable.name)
                });
                SnippetVariableTomlEntry::from_domain(variable, prior)
            })
            .collect();
        let context_bindings = definition
            .context_bindings
            .into_iter()
            .map(|binding| {
                let prior = previous.and_then(|document| {
                    document.context_bindings.iter().find(|entry| {
                        entry.kind == binding.kind && entry.target_id == binding.target_id
                    })
                });
                SnippetContextBindingTomlEntry::from_domain(binding, prior)
            })
            .collect();
        Self {
            schema_version: SCHEMA_VERSION,
            id: definition.id,
            title: definition.title,
            command: definition.command,
            description: definition.description,
            tags: definition.tags,
            scope: definition.scope,
            sort_order: definition.sort_order,
            created_at: previous
                .map_or_else(|| now.clone(), |document| document.created_at.clone()),
            updated_at: now,
            category: definition.category,
            risk: Some(definition.risk),
            default_action: Some(definition.default_action),
            variables,
            context_bindings,
            derived_from: definition.derived_from,
            extra_fields: previous
                .map_or_else(BTreeMap::new, |document| document.extra_fields.clone()),
        }
    }
}

/// 新建导入文件仅序列化当前 schema 拥有的字段，不继承 JSON 输入中的未知属性。
pub(super) fn serialize_new_snippet(definition: SnippetDefinition) -> AppResult<String> {
    let document = SnippetTomlDocument::new(definition, None);
    serialize_snippet_document(None, &document)
}

/// 补丁保留用户扩展和排版；写入前用正式 DTO 回读并核对文件元数据与领域语义，避免静默丢字段。
///
/// @author kongweiguang
fn serialize_snippet_document(
    previous_source: Option<&str>,
    replacement: &SnippetTomlDocument,
) -> AppResult<String> {
    let mut document = match previous_source {
        Some(source) => source.parse::<toml_edit::DocumentMut>().map_err(|source| {
            AppError::infrastructure_with_source("命令片段 TOML 无法安全补丁", source)
        })?,
        None => toml_edit::DocumentMut::new(),
    };
    let projected = toml_edit::ser::to_document(replacement)
        .map_err(|source| AppError::infrastructure_with_source("序列化命令片段失败", source))?;

    for key in [
        "schema_version",
        "id",
        "title",
        "command",
        "description",
        "tags",
        "scope",
        "sort_order",
        "created_at",
        "updated_at",
        "category",
        "risk",
        "default_action",
        "derived_from",
    ] {
        replace_item_preserving_decor(&mut document, key, projected.get(key));
    }
    patch_snippet_array_of_tables(
        &mut document,
        &projected,
        "variables",
        &[
            "name",
            "label",
            "description",
            "kind",
            "required",
            "default_value",
            "suggestions",
            "validation",
            "render_strategy",
            "sensitive",
        ],
        false,
    )?;
    patch_snippet_array_of_tables(
        &mut document,
        &projected,
        "context_bindings",
        &["kind", "target_id"],
        true,
    )?;

    let source = document.to_string();
    let readback: SnippetTomlDocument = toml::from_str(&source).map_err(|source| {
        AppError::infrastructure_with_source("命令片段序列化结果无法回读", source)
    })?;
    if readback.schema_version != SCHEMA_VERSION
        || readback.schema_version != replacement.schema_version
        || readback.id != replacement.id
        || readback.created_at != replacement.created_at
        || readback.updated_at != replacement.updated_at
        || to_definition(&readback) != to_definition(replacement)
    {
        return Err(AppError::infrastructure(
            "命令片段序列化结果与待写定义不一致",
        ));
    }
    Ok(source)
}

/// serde 投影将 Vec<struct> 表示为内联表数组；转成 AoT 后再按稳定身份保留旧注释和扩展键。
///
/// @author kongweiguang
fn patch_snippet_array_of_tables(
    document: &mut toml_edit::DocumentMut,
    projected: &toml_edit::DocumentMut,
    key: &str,
    owned_fields: &[&str],
    binding_identity: bool,
) -> AppResult<()> {
    let Some(projected_item) = projected.get(key) else {
        document.remove(key);
        return Ok(());
    };

    let new_tables = match projected_item {
        toml_edit::Item::ArrayOfTables(tables) => tables.iter().cloned().collect::<Vec<_>>(),
        toml_edit::Item::Value(value) => {
            let array = value
                .as_array()
                .ok_or_else(|| AppError::infrastructure("命令片段数组的序列化形态无效"))?;
            array
                .iter()
                .map(|value| {
                    value
                        .as_inline_table()
                        .map(|table| table.clone().into_table())
                        .ok_or_else(|| AppError::infrastructure("命令片段数组包含非表格项"))
                })
                .collect::<AppResult<Vec<_>>>()?
        }
        _ => return Err(AppError::infrastructure("命令片段数组的序列化形态无效")),
    };
    if new_tables.is_empty() {
        document.remove(key);
        return Ok(());
    }

    let old_tables = document
        .get(key)
        .and_then(toml_edit::Item::as_array_of_tables)
        .map(|tables| tables.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();

    let patched = new_tables
        .iter()
        .map(|new_table| {
            let stable_key = snippet_table_identity(new_table, binding_identity);
            let mut table = old_tables
                .iter()
                .find(|old_table| snippet_table_identity(old_table, binding_identity) == stable_key)
                .cloned()
                .unwrap_or_else(|| new_table.clone());
            for field in owned_fields {
                replace_table_item_preserving_decor(&mut table, field, new_table.get(field));
            }
            table
        })
        .collect::<toml_edit::ArrayOfTables>();
    document[key] = toml_edit::Item::ArrayOfTables(patched);
    Ok(())
}

/// 片段变量按名称、上下文绑定按 kind 与 target_id 对齐旧注释和扩展字段。
///
/// @author kongweiguang
fn snippet_table_identity(table: &toml_edit::Table, binding: bool) -> (String, Option<String>) {
    if binding {
        let kind = table
            .get("kind")
            .and_then(toml_edit::Item::as_str)
            .unwrap_or_default()
            .to_owned();
        let target = table
            .get("target_id")
            .and_then(toml_edit::Item::as_str)
            .map(str::to_owned);
        (kind, target)
    } else {
        (
            table
                .get("name")
                .and_then(toml_edit::Item::as_str)
                .unwrap_or_default()
                .to_owned(),
            None,
        )
    }
}

/// 替换根级已知字段时沿用旧值的 Decor，避免普通保存抹掉相邻说明与格式。
fn replace_item_preserving_decor(
    document: &mut toml_edit::DocumentMut,
    key: &str,
    replacement: Option<&toml_edit::Item>,
) {
    let Some(replacement) = replacement else {
        document.remove(key);
        return;
    };
    let mut replacement = replacement.clone();
    if let (Some(old), Some(new)) = (document.get(key), replacement.as_value_mut())
        && let Some(old) = old.as_value()
    {
        *new.decor_mut() = old.decor().clone();
    }
    document[key] = replacement;
}

/// 替换数组表中的已知字段而保留旧 Decor；删除字段只在新版 schema 明确归该字段所有时进行。
fn replace_table_item_preserving_decor(
    table: &mut toml_edit::Table,
    key: &str,
    replacement: Option<&toml_edit::Item>,
) {
    let Some(replacement) = replacement else {
        table.remove(key);
        return;
    };
    let mut replacement = replacement.clone();
    if let (Some(old), Some(new)) = (table.get(key), replacement.as_value_mut())
        && let Some(old) = old.as_value()
    {
        *new.decor_mut() = old.decor().clone();
    }
    table[key] = replacement;
}

/// 将合法 v1 文件转成与旧配置兼容的领域定义，缺失的扩展字段采用历史默认值。
fn to_definition(document: &SnippetTomlDocument) -> SnippetDefinition {
    SnippetDefinition {
        id: document.id.clone(),
        title: document.title.clone(),
        command: document.command.clone(),
        description: document.description.clone(),
        tags: document.tags.clone(),
        scope: document.scope,
        category: document.category.clone(),
        risk: document.risk.unwrap_or(SnippetRisk::Unknown),
        default_action: document
            .default_action
            .unwrap_or(SnippetDefaultAction::Insert),
        sort_order: document.sort_order,
        variables: document
            .variables
            .iter()
            .cloned()
            .map(SnippetVariableTomlEntry::into_domain)
            .collect(),
        context_bindings: document
            .context_bindings
            .iter()
            .cloned()
            .map(SnippetContextBindingTomlEntry::into_domain)
            .collect(),
        derived_from: document.derived_from.clone(),
    }
}

/// 构造界面目录投影，不给用户文件伪造内置来源、收藏或使用统计。
fn to_catalog_item(
    definition: SnippetDefinition,
    document: SnippetTomlDocument,
) -> SnippetCatalogItem {
    SnippetCatalogItem {
        id: definition.id,
        origin: SnippetCatalogOrigin::User,
        title: definition.title,
        description: definition.description.unwrap_or_default(),
        template: definition.command,
        category: definition.category.unwrap_or_else(|| "未分类".to_owned()),
        pack: "mine".to_owned(),
        tags: definition.tags,
        scope: definition.scope,
        platforms: Vec::new(),
        shells: Vec::new(),
        capabilities: Vec::new(),
        risk: definition.risk,
        sensitive: definition.variables.iter().any(|variable| {
            variable.sensitive
                || variable.kind == kerminal_domain::commands::SnippetVariableKind::Secret
        }),
        duration: String::new(),
        default_action: definition.default_action,
        variables: definition.variables,
        context_bindings: definition.context_bindings,
        catalog_version: None,
        source_name: None,
        source_url: None,
        command_spec: None,
        owner: None,
        tested_version: None,
        derived_from: definition.derived_from,
        deprecated: false,
        favorite: false,
        use_count: 0,
        last_used_at_unix_ms: None,
        sort_order: definition.sort_order,
        updated_at: document.updated_at,
    }
}

/// 目录按来源隔离，并保留旧 Any fallback 与忽略空白搜索词的语义。
fn snippet_matches(
    item: &SnippetCatalogItem,
    request: &SnippetCatalogListRequest,
    query: Option<&str>,
) -> bool {
    if request.origin.is_some_and(|origin| origin != item.origin)
        || request
            .scope
            .is_some_and(|scope| item.scope != scope && item.scope != SnippetScope::Any)
    {
        return false;
    }
    query.is_none_or(|query| {
        item.title.to_lowercase().contains(query)
            || item.description.to_lowercase().contains(query)
            || item.template.to_lowercase().contains(query)
            || item.category.to_lowercase().contains(query)
            || item
                .tags
                .iter()
                .any(|tag| tag.to_lowercase().contains(query))
            || item
                .capabilities
                .iter()
                .any(|capability| capability.to_lowercase().contains(query))
    })
}

/// 与 v0.3.45 目录顺序兼容：core 优先、用户片段其次，其它内置 pack 按名称排序。
fn pack_rank(pack: &str) -> u8 {
    match pack {
        "core" => 0,
        "mine" => 1,
        _ => 2,
    }
}

/// 使用系统时钟保存用户可见元数据；时钟异常时退回安全的零时刻字符串。
fn now_unix_millis() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}
