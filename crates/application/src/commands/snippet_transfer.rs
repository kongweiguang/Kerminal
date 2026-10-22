//! 旧 JSON v1 片段的预览、脱敏导出与可恢复批量导入用例。
//!
//! @author kongweiguang

use std::{
    collections::HashSet,
    io::{self, Write},
    time::{SystemTime, UNIX_EPOCH},
};

use kerminal_domain::commands::{
    SnippetCatalogItem, SnippetCatalogListRequest, SnippetCatalogOrigin, SnippetContextBinding,
    SnippetContextBindingKind, SnippetDefaultAction, SnippetDefinition, SnippetRisk, SnippetScope,
    SnippetVariable, SnippetVariableKind, SnippetVariableRenderStrategy,
};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use super::{
    AppError, AppResult, CommandsService, MAX_SNIPPET_TRANSFER_BYTES, MAX_SNIPPET_TRANSFER_ITEMS,
    SnippetRepository,
};

/// 仓储一次接收完整定义列表，负责 journal、顺序追加及文件 CreateNew 的原子提交。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnippetImportBatchRequest {
    pub batch_id: String,
    pub definitions: Vec<SnippetDefinition>,
}

/// 已确认提交的文件身份，顺序与本批所选项目一致。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnippetImportReceipt {
    pub batch_id: String,
    pub imported_ids: Vec<String>,
}

/// 仅返回可证明的提交结果；未知结果必须先查询，不得自动重放文件写入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnippetImportBatchState {
    NotCommitted,
    Committed(SnippetImportReceipt),
    OutcomeUnknown,
}

/// 预览只返回通过结构与领域校验的候选，定义 ID 已新建且不复用导出源身份。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnippetImportCandidate {
    pub source_index: usize,
    pub definition: SnippetDefinition,
    pub title_conflicts: bool,
}

/// 导入预览的安全诊断不回显命令正文、变量值或解析器错误内容。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnippetImportIssue {
    pub source_index: Option<usize>,
    pub message: String,
}

/// 预览同时保留有效候选和无效行说明，用户可以只选择本次确认的有效项。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SnippetImportPreview {
    pub candidates: Vec<SnippetImportCandidate>,
    pub issues: Vec<SnippetImportIssue>,
}

/// 读取用户目录生成兼容 JSON v1 文件，不导出内置项、运行值、收藏或使用统计。
/// @author kongweiguang
pub async fn export_user_snippets_json(repository: &dyn SnippetRepository) -> AppResult<String> {
    let items = list_all_user_snippets(repository).await?;
    if items.len() > MAX_SNIPPET_TRANSFER_ITEMS {
        return Err(AppError::invalid("片段导出超过单批 1,000 条限制"));
    }
    let snippets = items.iter().map(export_candidate).collect::<Vec<_>>();
    let document = SnippetTransferDocument {
        schema_version: 1,
        exported_at: utc_timestamp(),
        snippets,
    };
    let bytes = serialize_bounded_json(
        &document,
        true,
        "片段导出超过 32 MiB 限制",
        "命令片段导出序列化失败",
    )?;
    String::from_utf8(bytes).map_err(|_| AppError::infrastructure("命令片段导出编码失败"))
}

impl CommandsService {
    /// 只读解析旧 JSON v1，生成新 ID 并比对真实用户目录；预览本身不会写入文件。
    pub async fn preview_snippet_import(&self, source: &str) -> AppResult<SnippetImportPreview> {
        let existing = list_all_user_snippets(self.snippets.as_ref()).await?;
        Ok(preview_document(source, &existing))
    }

    /// 复核所选定义后委托同一个专用 journal；失败或未知提交不会由应用层逐文件重试。
    pub async fn import_snippet_batch(
        &self,
        request: SnippetImportBatchRequest,
    ) -> AppResult<SnippetImportBatchState> {
        validate_batch_request(&request)?;
        self.snippets.commit_import_batch(request).await
    }

    /// 对写入结果不明的导入只回读 journal 状态，避免重复创建同一批文件。
    pub async fn snippet_import_batch_status(
        &self,
        batch_id: String,
    ) -> AppResult<SnippetImportBatchState> {
        validate_batch_id(&batch_id)?;
        self.snippets.import_batch_status(batch_id).await
    }

    /// 导出只包含当前用户片段，并在 JSON v1 边界移除敏感变量默认值和建议值。
    pub async fn export_user_snippets(&self) -> AppResult<String> {
        export_user_snippets_json(self.snippets.as_ref()).await
    }
}

/// 导出和冲突预览必须看见完整用户目录；普通页面仍可请求较小的显示分页。
async fn list_all_user_snippets(
    repository: &dyn SnippetRepository,
) -> AppResult<Vec<SnippetCatalogItem>> {
    repository
        .list(SnippetCatalogListRequest {
            origin: Some(SnippetCatalogOrigin::User),
            limit: None,
            ..SnippetCatalogListRequest::default()
        })
        .await
}

/// 解析 legacy JSON v1，逐行拒绝无效条目而不让坏行伪装成可导入定义。
/// @author kongweiguang
fn preview_document(source: &str, existing: &[SnippetCatalogItem]) -> SnippetImportPreview {
    if source.len() > MAX_SNIPPET_TRANSFER_BYTES {
        return invalid_document("导入文件超过 32 MiB 限制");
    }
    let root: Value = match serde_json::from_str(source) {
        Ok(value) => value,
        Err(_) => return invalid_document("导入文件不是有效 JSON"),
    };
    let Some(object) = root.as_object() else {
        return invalid_document("导入文件版本或结构不受支持");
    };
    if object.get("schemaVersion").and_then(Value::as_u64) != Some(1) {
        return invalid_document("导入文件版本或结构不受支持");
    }
    let Some(rows) = object.get("snippets").and_then(Value::as_array) else {
        return invalid_document("导入文件版本或结构不受支持");
    };
    if rows.len() > MAX_SNIPPET_TRANSFER_ITEMS {
        return invalid_document("单次最多导入 1,000 个片段");
    }

    let mut preview = SnippetImportPreview::default();
    let mut titles = existing
        .iter()
        .map(|item| normalize_title(&item.title))
        .collect::<HashSet<_>>();
    for (index, row) in rows.iter().enumerate() {
        match parse_candidate(row, index) {
            Ok(definition) => {
                let title_conflicts = !titles.insert(normalize_title(&definition.title));
                preview.candidates.push(SnippetImportCandidate {
                    source_index: index,
                    definition,
                    title_conflicts,
                });
            }
            Err(message) => preview.issues.push(SnippetImportIssue {
                source_index: Some(index),
                message: message.to_owned(),
            }),
        }
    }
    preview
}

/// 比较目录标题时统一大小写并折叠首尾及连续空白，避免仅格式差异绕过重名提示。
fn normalize_title(title: &str) -> String {
    title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// 验证旧 transfer candidate 并生成新用户 ID；导出源 ID 不作为可写身份接收。
fn parse_candidate(value: &Value, source_index: usize) -> Result<SnippetDefinition, &'static str> {
    let Some(object) = value.as_object() else {
        return Err("不是对象");
    };
    let title = text(object.get("title"));
    let command = text(object.get("command"));
    if title.is_empty() || command.is_empty() {
        return Err("标题和命令不能为空");
    }
    if contains_sensitive_literal(&command) {
        return Err("命令包含疑似明文凭据");
    }
    let scope = match object.get("scope").and_then(Value::as_str) {
        Some("any") => SnippetScope::Any,
        Some("local") => SnippetScope::Local,
        Some("ssh") => SnippetScope::Ssh,
        _ => return Err("作用域无效"),
    };
    let risk = match object.get("risk").and_then(Value::as_str) {
        Some("inspect") => SnippetRisk::Inspect,
        Some("change") => SnippetRisk::Change,
        Some("destructive") => SnippetRisk::Destructive,
        Some("unknown") => SnippetRisk::Unknown,
        _ => return Err("风险类型无效"),
    };
    let default_action = match object.get("defaultAction").and_then(Value::as_str) {
        Some("insert") => SnippetDefaultAction::Insert,
        Some("run") => SnippetDefaultAction::Run,
        _ => return Err("默认动作无效"),
    };
    let variables = parse_variables(object.get("variables"))?;
    let context_bindings = parse_context_bindings(object.get("contextBindings"))?;
    let tags = object
        .get("tags")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .take(12)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let definition = SnippetDefinition {
        id: uuid::Uuid::new_v4().to_string(),
        title,
        command,
        description: Some(text(object.get("description"))),
        tags,
        // 存储适配器在批次锁内将导入项追加到目录末尾并保留请求次序。
        sort_order: source_index as i64,
        scope,
        category: Some(or_default_text(object.get("category"), "custom")),
        risk,
        default_action,
        variables,
        context_bindings,
        derived_from: optional_text(object.get("derivedFrom")),
    };
    definition
        .validate()
        .map_err(|_| "片段字段、变量或模板占位符未通过校验")?;
    Ok(definition)
}

/// 保留旧前端默认空变量数组的行为，同时严格校验有内容的变量类型和安全值。
fn parse_variables(value: Option<&Value>) -> Result<Vec<SnippetVariable>, &'static str> {
    let Some(rows) = value.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    rows.iter().map(parse_variable).collect()
}

/// 解析单个变量，避免不完整的 transfer 字段在执行时被当作可信输入。
fn parse_variable(value: &Value) -> Result<SnippetVariable, &'static str> {
    let Some(object) = value.as_object() else {
        return Err("变量定义无效或包含敏感默认值");
    };
    let kind = match object.get("kind").and_then(Value::as_str) {
        Some("text") => SnippetVariableKind::Text,
        Some("path") => SnippetVariableKind::Path,
        Some("port") => SnippetVariableKind::Port,
        Some("integer") => SnippetVariableKind::Integer,
        Some("host") => SnippetVariableKind::Host,
        Some("url") => SnippetVariableKind::Url,
        Some("service") => SnippetVariableKind::Service,
        Some("container") => SnippetVariableKind::Container,
        Some("enum") => SnippetVariableKind::Enum,
        Some("secret") => SnippetVariableKind::Secret,
        Some("raw") => SnippetVariableKind::Raw,
        _ => return Err("变量定义无效或包含敏感默认值"),
    };
    let render_strategy = match object.get("renderStrategy").and_then(Value::as_str) {
        Some("shellArg") => SnippetVariableRenderStrategy::ShellArg,
        Some("validatedRaw") => SnippetVariableRenderStrategy::ValidatedRaw,
        Some("literal") => SnippetVariableRenderStrategy::Literal,
        _ => return Err("变量定义无效或包含敏感默认值"),
    };
    let suggestions = object
        .get("suggestions")
        .and_then(Value::as_array)
        .filter(|items| items.iter().all(Value::is_string))
        .ok_or("变量定义无效或包含敏感默认值")?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let default_value = optional_raw_text(object.get("defaultValue"))?;
    let sensitive = object
        .get("sensitive")
        .and_then(Value::as_bool)
        .ok_or("变量定义无效或包含敏感默认值")?;
    let variable = SnippetVariable {
        name: text(object.get("name")),
        label: text(object.get("label")),
        description: text(object.get("description")),
        kind,
        required: object
            .get("required")
            .and_then(Value::as_bool)
            .ok_or("变量定义无效或包含敏感默认值")?,
        default_value,
        suggestions,
        validation: optional_raw_text(object.get("validation"))?,
        render_strategy,
        sensitive,
    };
    if (kind == SnippetVariableKind::Secret || sensitive)
        && (variable
            .default_value
            .as_deref()
            .is_some_and(|value| !value.is_empty())
            || !variable.suggestions.is_empty())
    {
        return Err("变量定义无效或包含敏感默认值");
    }
    Ok(variable)
}

/// 绑定数组缺失时兼容旧 JSON v1 空值，存在的绑定则必须有受支持的类型。
fn parse_context_bindings(
    value: Option<&Value>,
) -> Result<Vec<SnippetContextBinding>, &'static str> {
    let Some(rows) = value.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    rows.iter()
        .map(|row| {
            let Some(object) = row.as_object() else {
                return Err("上下文绑定无效");
            };
            let kind = match object.get("kind").and_then(Value::as_str) {
                Some("global") => SnippetContextBindingKind::Global,
                Some("workspace") => SnippetContextBindingKind::Workspace,
                Some("host") => SnippetContextBindingKind::Host,
                Some("hostGroup") => SnippetContextBindingKind::HostGroup,
                _ => return Err("上下文绑定无效"),
            };
            let target_id = optional_raw_text(object.get("targetId"))?;
            Ok(SnippetContextBinding { kind, target_id })
        })
        .collect()
}

/// 把受选用户定义写入 JSON v1 结构，并清除敏感变量默认值及建议值。
/// @author kongweiguang
fn export_candidate(item: &SnippetCatalogItem) -> SnippetTransferCandidate {
    let variables = item
        .variables
        .iter()
        .map(|variable| {
            let sensitive = variable.kind == SnippetVariableKind::Secret || variable.sensitive;
            SnippetTransferVariable {
                name: variable.name.clone(),
                label: variable.label.clone(),
                description: variable.description.clone(),
                kind: variable.kind,
                required: variable.required,
                // 旧 JSON v1 会省略敏感 defaultValue；普通缺省值仍按旧格式输出 null。
                default_value: (!sensitive).then(|| variable.default_value.clone()),
                suggestions: if sensitive {
                    Vec::new()
                } else {
                    variable.suggestions.clone()
                },
                validation: variable.validation.clone(),
                render_strategy: variable.render_strategy,
                sensitive: variable.sensitive,
            }
        })
        .collect();
    SnippetTransferCandidate {
        title: item.title.clone(),
        command: item.template.clone(),
        description: item.description.clone(),
        tags: item.tags.clone(),
        scope: item.scope,
        category: item.category.clone(),
        risk: item.risk,
        default_action: item.default_action,
        variables,
        context_bindings: item.context_bindings.clone(),
        derived_from: item.derived_from.clone(),
    }
}

/// 对导入候选重新验证批次身份、数量、重复 ID 和敏感正文，防止 UI DTO 被直接伪造。
/// @author kongweiguang
fn validate_batch_request(request: &SnippetImportBatchRequest) -> AppResult<()> {
    validate_batch_id(&request.batch_id)?;
    if request.definitions.is_empty() || request.definitions.len() > MAX_SNIPPET_TRANSFER_ITEMS {
        return Err(AppError::invalid("导入片段数量无效"));
    }
    let mut ids = HashSet::with_capacity(request.definitions.len());
    for definition in &request.definitions {
        if !ids.insert(definition.id.as_str()) {
            return Err(AppError::invalid("导入片段标识重复"));
        }
        validate_transfer_definition(definition)?;
    }
    let _ = serialize_bounded_json(
        &request.definitions,
        false,
        "片段导入批次超过 32 MiB 限制",
        "命令片段批次序列化失败",
    )?;
    Ok(())
}

/// 仅接受短且无路径或控制字符语义的 batch ID，避免 journal key 进入文件名边界。
fn validate_batch_id(batch_id: &str) -> AppResult<()> {
    if batch_id.is_empty()
        || batch_id.chars().count() > 160
        || batch_id
            .chars()
            .any(|value| value.is_control() || matches!(value, '/' | '\\'))
    {
        return Err(AppError::invalid("片段导入批次标识无效"));
    }
    Ok(())
}

/// 写盘前再次拒绝敏感字面量与无效模板，避免预览结果成为安全校验的唯一边界。
fn validate_transfer_definition(definition: &SnippetDefinition) -> AppResult<()> {
    definition
        .validate()
        .map_err(|_| AppError::invalid("导入片段未通过定义校验"))?;
    if contains_sensitive_literal(&definition.command) {
        return Err(AppError::invalid("导入片段命令包含疑似明文凭据"));
    }
    if definition.id.trim().is_empty()
        || definition.id.chars().count() > 160
        || definition
            .id
            .chars()
            .any(|value| value.is_control() || matches!(value, '/' | '\\'))
    {
        return Err(AppError::invalid("导入片段标识无效"));
    }
    Ok(())
}

/// 与旧 JSON v1 使用同一组凭据字面量规则，但错误只返回固定类别、不回显命中内容。
fn contains_sensitive_literal(command: &str) -> bool {
    let command = command.replace("{{ ", "{{").replace(" }}", "}}");
    static PRIVATE_KEY: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)-----BEGIN [A-Z ]*PRIVATE KEY-----").expect("固定私钥扫描表达式必须可编译")
    });
    static BEARER: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)\bBearer\s+([^\s'";]{8,})"#).expect("固定 bearer 扫描表达式必须可编译")
    });
    static ASSIGNMENT: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)\b(password|passwd|token|api[_-]?key|secret|key[_-]?passphrase)\s*[:=]\s*([^\s'";,]{4,})"#)
            .expect("固定凭据赋值扫描表达式必须可编译")
    });
    static PROVIDER_TOKEN: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"\b(?:sk|ghp|github_pat|xox[baprs])[-_][A-Za-z0-9_-]{12,}\b")
            .expect("固定服务令牌扫描表达式必须可编译")
    });
    let placeholders =
        Regex::new(r"\{\{\s*[A-Za-z][A-Za-z0-9_]*\s*\}\}").expect("固定模板变量表达式必须可编译");
    let scrubbed = placeholders.replace_all(&command, "[variable]");
    PRIVATE_KEY.is_match(&scrubbed)
        || BEARER
            .captures(&scrubbed)
            .and_then(|captures| captures.get(1))
            .is_some_and(|token| token.as_str() != "[variable]")
        || ASSIGNMENT
            .captures(&scrubbed)
            .and_then(|captures| captures.get(2))
            .is_some_and(|value| value.as_str() != "[variable]")
        || PROVIDER_TOKEN.is_match(&scrubbed)
}

/// 把结构错误作为不含源内容的预览 issue 返回，便于 UI 保持可取消的文件选择流程。
fn invalid_document(message: &str) -> SnippetImportPreview {
    SnippetImportPreview {
        candidates: Vec::new(),
        issues: vec![SnippetImportIssue {
            source_index: None,
            message: message.to_owned(),
        }],
    }
}

/// 输出与旧前端相同的 JSON v1 候选字段，不泄漏来源 ID 或目录统计。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SnippetTransferDocument {
    schema_version: u32,
    exported_at: String,
    snippets: Vec<SnippetTransferCandidate>,
}

/// 传输候选不携带存储 ID、收藏状态和使用数据。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SnippetTransferCandidate {
    title: String,
    command: String,
    description: String,
    tags: Vec<String>,
    scope: SnippetScope,
    category: String,
    risk: SnippetRisk,
    default_action: SnippetDefaultAction,
    variables: Vec<SnippetTransferVariable>,
    context_bindings: Vec<SnippetContextBinding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    derived_from: Option<String>,
}

/// JSON v1 导出投影用嵌套 Option 区分敏感字段省略和普通 null 缺省值。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SnippetTransferVariable {
    name: String,
    label: String,
    description: String,
    kind: SnippetVariableKind,
    required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_value: Option<Option<String>>,
    suggestions: Vec<String>,
    validation: Option<String>,
    render_strategy: SnippetVariableRenderStrategy,
    sensitive: bool,
}

/// 限制 JSON 序列化实际输出字节，避免先生成超大 String 再检查上限。
/// @author kongweiguang
fn serialize_bounded_json<T: Serialize>(
    value: &T,
    pretty: bool,
    too_large: &'static str,
    serialization_error: &'static str,
) -> AppResult<Vec<u8>> {
    let mut writer = BoundedJsonWriter::new(MAX_SNIPPET_TRANSFER_BYTES);
    let result = if pretty {
        serde_json::to_writer_pretty(&mut writer, value)
    } else {
        serde_json::to_writer(&mut writer, value)
    };
    if result.is_err() {
        return if writer.exceeded {
            Err(AppError::invalid(too_large))
        } else {
            Err(AppError::infrastructure(serialization_error))
        };
    }
    Ok(writer.bytes)
}

/// 按真实序列化字节预算收集 JSON，遇到超限立即停止写入而不返回半份文件。
struct BoundedJsonWriter {
    bytes: Vec<u8>,
    max_bytes: usize,
    exceeded: bool,
}

impl BoundedJsonWriter {
    /// 小容量起步并在每次写入时执行统一预算，避免目录大小决定初始大额分配。
    /// @author kongweiguang
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(4 * 1024),
            max_bytes,
            exceeded: false,
        }
    }
}

impl Write for BoundedJsonWriter {
    /// 只有整个写入块能放进预算时才追加，序列化失败后调用方丢弃临时缓冲。
    /// @author kongweiguang
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() > self.max_bytes.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snippet JSON byte budget exceeded",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    /// 内存 writer 没有待提交的外部缓冲，保留标准 Write 契约即可。
    /// @author kongweiguang
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 旧 UI 会忽略时间以外的顶层扩展；这里仍输出 ISO UTC `exportedAt` 以保持文件外观兼容。
fn utc_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.000Z")
}

/// 使用公历 400 年周期把 Unix 日号转换为日期，避免为可选导出时间引入运行依赖。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

/// 读取字符串字段时仅对展示元数据 trim；变量默认值由独立原始值解析保留。
fn text(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_owned()
}

/// category 延续旧导入器：缺失或空字符串使用 `custom`。
fn or_default_text(value: Option<&Value>, default: &str) -> String {
    let value = text(value);
    if value.is_empty() {
        default.to_owned()
    } else {
        value
    }
}

/// 可选 metadata 缺失或非字符串时按旧 transfer 语义忽略。
fn optional_text(value: Option<&Value>) -> Option<String> {
    let value = text(value);
    (!value.is_empty()).then_some(value)
}

/// 变量运行值不做 trim，但拒绝非字符串字段，避免静默改变用户预设值。
fn optional_raw_text(value: Option<&Value>) -> Result<Option<String>, &'static str> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => Err("变量定义无效或包含敏感默认值"),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/snippet_transfer.rs"]
mod tests;
