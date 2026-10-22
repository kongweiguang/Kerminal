//! Agent 工作区的一次性 0.4 数据迁移规划；普通 Agent reader 不再解析旧 live 引用。
//!
//! @author kongweiguang

use std::{
    fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use kerminal_application::{
    AppError, AppResult,
    agents::{builtin_resume_argv, initial_provider_session},
};
use kerminal_domain::{
    agents::{
        AgentDomainError, AgentLauncherKey, AgentProvider, AgentProviderSession, AgentSession,
        AgentSessionLaunch, AgentSessionScope, AgentSessionStatus, AgentTargetBinding,
        AgentTargetLiveStatus,
    },
    identity::{AgentSessionId, PaneId, SessionId, TabId},
};
use serde::Deserialize;
use serde_json::{Map, Value};
use toml_edit::{DocumentMut, Item, TableLike, Value as TomlValue, value};

use super::FileEdit;

const SESSIONS_RELATIVE: &str = "agents/sessions";
const JOURNAL_FILE: &str = ".agent-workspace-transaction.json";
const STAGE_SUFFIX: &str = ".agent-stage";
const MAX_SESSION_COUNT: usize = 16_384;
const MAX_SESSION_FILE_BYTES: u64 = 1024 * 1024;
const MAX_CONTEXT_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TOTAL_READ_BYTES: usize = 128 * 1024 * 1024;

/// 在一次性转换中解码 scope 历史别名；普通 Store 与领域当前模型只接受规范字段名。
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum AgentSessionScopeDocument {
    Tab {
        #[serde(rename = "tabId", alias = "tab_id")]
        tab_id: TabId,
    },
    Global,
}

impl From<AgentSessionScopeDocument> for AgentSessionScope {
    /// 将迁移 DTO 转成当前领域归属值，不把旧别名带入普通运行路径。
    ///
    /// @author kongweiguang
    fn from(scope: AgentSessionScopeDocument) -> Self {
        match scope {
            AgentSessionScopeDocument::Tab { tab_id } => Self::Tab { tab_id },
            AgentSessionScopeDocument::Global => Self::Global,
        }
    }
}

/// 与当前 Store 的持久 DTO 对齐，以领域恢复入口校验业务不变量，再由 toml_edit 原地规范化。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentSessionDocument {
    schema_version: u32,
    agent_session_id: AgentSessionId,
    agent_id: AgentProvider,
    #[serde(default)]
    launcher_key: Option<AgentLauncherKey>,
    title: String,
    created_at: String,
    updated_at: String,
    status: AgentSessionStatus,
    workspace_root: String,
    session_root: String,
    launch: AgentSessionLaunch,
    #[serde(default)]
    scope: Option<AgentSessionScopeDocument>,
    #[serde(default)]
    target: Option<AgentSessionTargetDocument>,
}

impl AgentSessionDocument {
    /// 复用 AgentSession::restore 校验 schema、launch、provider key 和 Ready 目标定义。
    ///
    /// @author kongweiguang
    fn restore(self) -> Result<AgentSession, AgentDomainError> {
        AgentSession::restore(
            self.schema_version,
            self.agent_session_id,
            self.agent_id,
            self.launcher_key,
            self.title,
            self.created_at,
            self.updated_at,
            self.status,
            self.workspace_root,
            self.session_root,
            self.launch,
            self.scope.map(AgentSessionScope::from),
            self.target.map(AgentTargetBinding::from),
        )
    }
}

/// session.toml 的旧稳定目标键名只在迁移边界归一化到当前领域值对象。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentSessionTargetDocument {
    #[serde(default, alias = "bindingId")]
    binding_id: Option<String>,
    #[serde(
        default,
        rename = "binding_generation",
        alias = "generation",
        alias = "bindingGeneration"
    )]
    generation: u64,
    #[serde(default, alias = "paneId")]
    pane_id: Option<PaneId>,
    #[serde(default, alias = "tabId")]
    tab_id: Option<TabId>,
    #[serde(
        default,
        rename = "target_terminal_session_id",
        alias = "terminal_session_id",
        alias = "targetTerminalSessionId",
        alias = "terminalSessionId"
    )]
    terminal_session_id: Option<SessionId>,
    #[serde(default, alias = "targetRef")]
    target_ref: Option<String>,
    #[serde(default, alias = "targetKind")]
    target_kind: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    shell: Option<String>,
    #[serde(alias = "liveStatus")]
    live_status: AgentTargetLiveStatus,
    #[serde(default, alias = "lastSeenAt")]
    last_seen_at: Option<String>,
}

impl From<AgentSessionTargetDocument> for AgentTargetBinding {
    /// 保留目标定义和显式 terminal id；迁移只作领域校验，不把定义误当作 live 状态。
    ///
    /// @author kongweiguang
    fn from(target: AgentSessionTargetDocument) -> Self {
        Self {
            binding_id: target.binding_id,
            generation: target.generation,
            pane_id: target.pane_id,
            tab_id: target.tab_id,
            terminal_session_id: target.terminal_session_id,
            target_ref: target.target_ref,
            target_kind: target.target_kind,
            cwd: target.cwd,
            shell: target.shell,
            live_status: target.live_status,
            last_seen_at: target.last_seen_at,
        }
    }
}

/// 按固定 Agent 路径生成一组 CAS 前后字节；函数只读磁盘，提交、备份与恢复由 migration core 统一负责。
///
/// @author kongweiguang
pub(super) fn plan_agent_migration(root: &Path) -> AppResult<Vec<FileEdit>> {
    let Some(canonical_root) = canonical_root_if_present(root)? else {
        return Ok(Vec::new());
    };
    let Some(agents_root) = existing_directory(&canonical_root.join("agents"), &canonical_root)?
    else {
        return Ok(Vec::new());
    };
    let Some(sessions_root) = existing_directory(&agents_root.join("sessions"), &canonical_root)?
    else {
        return Ok(Vec::new());
    };

    let mut entries = Vec::new();
    for entry in fs::read_dir(&sessions_root).map_err(migration_io_error)? {
        let entry = entry.map_err(migration_io_error)?;
        if entries.len() >= MAX_SESSION_COUNT {
            return Err(migration_corrupt());
        }
        entries.push(entry);
    }
    entries.sort_by_key(|entry| entry.file_name());

    let mut edits = Vec::new();
    let mut read_bytes = 0usize;
    for entry in entries {
        let file_type = entry.file_type().map_err(migration_io_error)?;
        if file_type.is_symlink() {
            return Err(migration_corrupt());
        }
        if !file_type.is_dir() {
            continue;
        }
        let raw_id = entry
            .file_name()
            .into_string()
            .map_err(|_| migration_corrupt())?;
        let id = AgentSessionId::parse(&raw_id).map_err(|_| migration_corrupt())?;
        validate_session_path_id(&id)?;
        let session_root = fs::canonicalize(entry.path()).map_err(migration_io_error)?;
        if !session_root.starts_with(&canonical_root) || !session_root.starts_with(&sessions_root) {
            return Err(migration_corrupt());
        }
        reject_pending_agent_transaction(&session_root, &canonical_root)?;
        plan_one_session(
            &canonical_root,
            &session_root,
            &id,
            &mut read_bytes,
            &mut edits,
        )?;
    }
    Ok(edits)
}

/// 只接受可作为单一路径组件的 session id，保持 Store 的旧标识语义并阻断路径跳转。
///
/// @author kongweiguang
fn validate_session_path_id(id: &AgentSessionId) -> AppResult<()> {
    let value = id.as_str();
    let mut components = Path::new(value).components();
    if value.trim().is_empty()
        || value.contains('/')
        || value.contains('\\')
        || value.contains(':')
        || value.chars().any(char::is_control)
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(migration_corrupt());
    }
    Ok(())
}

/// 校验固定目录不是符号链接且仍位于配置根内；缺失目录只表示没有可迁移 Agent 数据。
///
/// @author kongweiguang
fn existing_directory(path: &Path, canonical_root: &Path) -> AppResult<Option<PathBuf>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(migration_io_error(error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(migration_corrupt());
    }
    let canonical = fs::canonicalize(path).map_err(migration_io_error)?;
    if !canonical.starts_with(canonical_root) {
        return Err(migration_corrupt());
    }
    Ok(Some(canonical))
}

/// 读取配置根时不创建目录；不存在的根由 core 继续处理其他迁移器。
///
/// @author kongweiguang
fn canonical_root_if_present(root: &Path) -> AppResult<Option<PathBuf>> {
    match fs::canonicalize(root) {
        Ok(path) => {
            let metadata = fs::metadata(&path).map_err(migration_io_error)?;
            if !metadata.is_dir() {
                return Err(migration_corrupt());
            }
            Ok(Some(path))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(migration_io_error(error)),
    }
}

/// 一个 session 内只读取固定元数据/context 文件，并先拒绝未恢复的 Agent Store journal 或 staging。
///
/// @author kongweiguang
fn reject_pending_agent_transaction(session_root: &Path, canonical_root: &Path) -> AppResult<()> {
    let journal = session_root.join(JOURNAL_FILE);
    match fs::symlink_metadata(&journal) {
        Ok(_) => return Err(migration_corrupt()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(migration_io_error(error)),
    }
    if contains_agent_stage(session_root)? {
        return Err(migration_corrupt());
    }
    if let Some(context_root) = existing_directory(&session_root.join("context"), canonical_root)?
        && contains_agent_stage(&context_root)?
    {
        return Err(migration_corrupt());
    }
    Ok(())
}

/// 只检查 Agent Store 保留的 stage 后缀，不递归扫描工作区或第三方 Agent 文件。
///
/// @author kongweiguang
fn contains_agent_stage(directory: &Path) -> AppResult<bool> {
    for entry in fs::read_dir(directory).map_err(migration_io_error)? {
        let entry = entry.map_err(migration_io_error)?;
        let name = entry.file_name();
        if name.to_string_lossy().ends_with(STAGE_SUFFIX) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 校验当前 session/provider 身份，并把目标字段别名与运行态引用一起生成精确 CAS edit。
///
/// @author kongweiguang
fn plan_one_session(
    canonical_root: &Path,
    session_root: &Path,
    expected_id: &AgentSessionId,
    read_bytes: &mut usize,
    edits: &mut Vec<FileEdit>,
) -> AppResult<()> {
    let session_bytes = read_required_document(
        canonical_root,
        session_root,
        Path::new("session.toml"),
        MAX_SESSION_FILE_BYTES,
        read_bytes,
    )?;
    let session_text = std::str::from_utf8(&session_bytes).map_err(|_| migration_corrupt())?;
    let session_file: AgentSessionDocument =
        toml::from_str(session_text).map_err(|_| migration_corrupt())?;
    let scope_was_missing = session_file.scope.is_none();
    let mut session = session_file.restore().map_err(|_| migration_corrupt())?;
    if session.agent_session_id.as_str() != expected_id.as_str() {
        return Err(migration_corrupt());
    }
    if let Some(provider_edit) = provider_document_migration(
        canonical_root,
        session_root,
        expected_id,
        &session,
        read_bytes,
    )? {
        edits.push(provider_edit);
    }

    if scope_was_missing {
        let scope = session
            .target
            .as_ref()
            .and_then(|target| target.tab_id.clone())
            .map(|tab_id| AgentSessionScope::Tab { tab_id })
            .unwrap_or(AgentSessionScope::Global);
        session.scope = Some(scope);
    }

    let mut document = session_text
        .parse::<DocumentMut>()
        .map_err(|_| migration_corrupt())?;
    let mut session_changed = normalize_target_aliases(&mut document)?;
    session_changed |= normalize_session_scope(
        &mut document,
        session.scope.as_ref().ok_or_else(migration_corrupt)?,
        scope_was_missing,
    )?;
    if session.status == AgentSessionStatus::Active {
        session.mark_cli_stale(unix_timestamp_seconds()?);
        document["status"] = value("stale");
        document["updated_at"] = value(session.updated_at.clone());
        session_changed = true;
    }
    if session_changed {
        edits.push(file_edit(
            expected_id,
            Path::new("session.toml"),
            session_bytes,
            document.to_string().into_bytes(),
        ));
    }

    plan_context_edit(
        canonical_root,
        session_root,
        expected_id,
        Path::new("context/mcp-endpoint.json"),
        read_bytes,
        edits,
        migrate_mcp_endpoint_context,
    )?;
    plan_context_edit(
        canonical_root,
        session_root,
        expected_id,
        Path::new("context/target-binding.json"),
        read_bytes,
        edits,
        migrate_target_binding_context,
    )?;
    plan_context_edit(
        canonical_root,
        session_root,
        expected_id,
        Path::new("context/terminal-snapshot.json"),
        read_bytes,
        edits,
        migrate_terminal_snapshot_context,
    )?;
    Ok(())
}

/// 仅在迁移边界把 session.toml target 的旧字段名改成 store 当前严格接受的 snake_case。
///
/// 双写同一语义字段时整份迁移失败；搬移 TOML 节点而不是重建文档，以保留值与装饰。
///
/// @author kongweiguang
fn normalize_target_aliases(document: &mut DocumentMut) -> AppResult<bool> {
    let Some(target) = document.as_table_mut().get_mut("target") else {
        return Ok(false);
    };
    match target {
        Item::Table(table) => normalize_target_table_aliases(table),
        Item::Value(TomlValue::InlineTable(table)) => normalize_target_table_aliases(table),
        _ => Err(migration_corrupt()),
    }
}

/// 归一化所有已知历史 target 键；不触碰未知字段，且发现多个别名或双键时拒绝清理。
///
/// @author kongweiguang
fn normalize_target_table_aliases(table: &mut impl TableLike) -> AppResult<bool> {
    const ALIASES: [(&str, &[&str]); 9] = [
        ("binding_id", &["bindingId"]),
        ("binding_generation", &["generation", "bindingGeneration"]),
        ("pane_id", &["paneId"]),
        ("tab_id", &["tabId"]),
        (
            "target_terminal_session_id",
            &[
                "terminal_session_id",
                "targetTerminalSessionId",
                "terminalSessionId",
            ],
        ),
        ("target_ref", &["targetRef"]),
        ("target_kind", &["targetKind"]),
        ("live_status", &["liveStatus"]),
        ("last_seen_at", &["lastSeenAt"]),
    ];

    let mut changed = false;
    for (canonical, aliases) in ALIASES {
        let present_aliases = aliases
            .iter()
            .copied()
            .filter(|alias| table.contains_key(alias))
            .collect::<Vec<_>>();
        if present_aliases.is_empty() {
            continue;
        }
        if table.contains_key(canonical) || present_aliases.len() != 1 {
            return Err(migration_corrupt());
        }

        let alias = present_aliases[0];
        let decor = table
            .key(alias)
            .ok_or_else(migration_corrupt)?
            .leaf_decor()
            .clone();
        let item = table.remove(alias).ok_or_else(migration_corrupt)?;
        if table.insert(canonical, item).is_some() {
            return Err(migration_corrupt());
        }
        let mut canonical_key = table.key_mut(canonical).ok_or_else(migration_corrupt)?;
        *canonical_key.leaf_decor_mut() = decor;
        changed = true;
    }
    Ok(changed)
}

/// 将旧 scope 的 tab_id 改为规范 tabId；缺少字段时只从此文件的已存目标归属迁移。
///
/// 双写或非法表结构拒绝迁移；插入缺失字段时写出当前领域序列化形式。
///
/// @author kongweiguang
fn normalize_session_scope(
    document: &mut DocumentMut,
    scope: &AgentSessionScope,
    missing: bool,
) -> AppResult<bool> {
    if missing {
        if document.as_table().contains_key("scope") {
            return Err(migration_corrupt());
        }
        let mut table = toml_edit::InlineTable::new();
        match scope {
            AgentSessionScope::Global => {
                table.insert("kind", TomlValue::from("global"));
            }
            AgentSessionScope::Tab { tab_id } => {
                table.insert("kind", TomlValue::from("tab"));
                table.insert("tabId", TomlValue::from(tab_id.as_str()));
            }
        }
        document["scope"] = Item::Value(TomlValue::InlineTable(table));
        return Ok(true);
    }

    let scope_item = document
        .as_table_mut()
        .get_mut("scope")
        .ok_or_else(migration_corrupt)?;
    match scope_item {
        Item::Table(table) => normalize_scope_table_alias(table),
        Item::Value(TomlValue::InlineTable(table)) => normalize_scope_table_alias(table),
        _ => Err(migration_corrupt()),
    }
}

/// 仅移动 scope 的历史字段节点，保留 TOML 注释并拒绝双写。
///
/// @author kongweiguang
fn normalize_scope_table_alias(table: &mut impl TableLike) -> AppResult<bool> {
    if !table.contains_key("tab_id") {
        return Ok(false);
    }
    if table.contains_key("tabId") {
        return Err(migration_corrupt());
    }
    let decor = table
        .key("tab_id")
        .ok_or_else(migration_corrupt)?
        .leaf_decor()
        .clone();
    let item = table.remove("tab_id").ok_or_else(migration_corrupt)?;
    if table.insert("tabId", item).is_some() {
        return Err(migration_corrupt());
    }
    *table
        .key_mut("tabId")
        .ok_or_else(migration_corrupt)?
        .leaf_decor_mut() = decor;
    Ok(true)
}

/// 一次迁移创建缺失 provider 文件，或补齐可由当前启动策略恢复的 builtin 字段。
///
/// @author kongweiguang
fn provider_document_migration(
    canonical_root: &Path,
    session_root: &Path,
    expected_id: &AgentSessionId,
    session: &AgentSession,
    read_bytes: &mut usize,
) -> AppResult<Option<FileEdit>> {
    let before = read_optional_document(
        canonical_root,
        session_root,
        Path::new("provider.toml"),
        MAX_SESSION_FILE_BYTES,
        read_bytes,
    )?;
    let was_missing = before.is_none();
    let mut provider = if let Some(bytes) = before.as_ref() {
        let text = std::str::from_utf8(bytes).map_err(|_| migration_corrupt())?;
        toml::from_str::<AgentProviderSession>(text).map_err(|_| migration_corrupt())?
    } else {
        initial_provider_session(session.agent_id)
    };
    provider.validate().map_err(|_| migration_corrupt())?;
    if provider.provider != session.agent_id {
        return Err(migration_corrupt());
    }
    let normalized = if was_missing {
        false
    } else {
        normalize_missing_builtin_resume_command(&mut provider)?
    };
    if !was_missing && !normalized {
        return Ok(None);
    }
    let serialized = toml::to_string_pretty(&provider)
        .map_err(|error| {
            AppError::infrastructure_with_source("Agent provider migration encoding failed", error)
        })?
        .into_bytes();
    let edit = if let Some(before) = before {
        file_edit(expected_id, Path::new("provider.toml"), before, serialized)
    } else {
        file_creation_edit(expected_id, Path::new("provider.toml"), serialized)
    };
    Ok(Some(edit))
}

/// 只在迁移边界归一化缺失命令；先用当前 argv 策略验证 provider ID，避免写入不可恢复的 Pi 状态。
///
/// @author kongweiguang
fn normalize_missing_builtin_resume_command(
    provider: &mut AgentProviderSession,
) -> AppResult<bool> {
    if provider.provider == AgentProvider::Custom
        || !provider.resume_supported
        || provider.resume_command.is_some()
    {
        return Ok(false);
    }
    let Some(command) = initial_provider_session(provider.provider).resume_command else {
        return Err(migration_corrupt());
    };
    builtin_resume_argv(provider.provider, provider.provider_session_id.as_deref())?;
    provider.resume_command = Some(command);
    Ok(true)
}

/// 读取固定文档并在需要改写时保留原字节作为 core 的 CAS 基线。
///
/// @author kongweiguang
fn plan_context_edit(
    canonical_root: &Path,
    session_root: &Path,
    id: &AgentSessionId,
    relative_path: &Path,
    read_bytes: &mut usize,
    edits: &mut Vec<FileEdit>,
    transform: impl FnOnce(&[u8], &AgentSessionId) -> AppResult<Option<Vec<u8>>>,
) -> AppResult<()> {
    let Some(before) = read_optional_document(
        canonical_root,
        session_root,
        relative_path,
        MAX_CONTEXT_FILE_BYTES,
        read_bytes,
    )?
    else {
        return Ok(());
    };
    if let Some(after) = transform(&before, id)? {
        edits.push(file_edit(id, relative_path, before, after));
    }
    Ok(())
}

/// 仅按固定相对路径读取普通文件，验证大小和 canonical containment，不创建目录或写入任何内容。
///
/// @author kongweiguang
fn read_required_document(
    canonical_root: &Path,
    session_root: &Path,
    relative_path: &Path,
    maximum_bytes: u64,
    total_bytes: &mut usize,
) -> AppResult<Vec<u8>> {
    read_optional_document(
        canonical_root,
        session_root,
        relative_path,
        maximum_bytes,
        total_bytes,
    )?
    .ok_or_else(migration_corrupt)
}

/// 缺失固定 context/provider 文件保持缺失；任何已存在的非普通文件、超限或越界路径均失败。
///
/// @author kongweiguang
fn read_optional_document(
    canonical_root: &Path,
    session_root: &Path,
    relative_path: &Path,
    maximum_bytes: u64,
    total_bytes: &mut usize,
) -> AppResult<Option<Vec<u8>>> {
    let mut current = session_root.to_path_buf();
    let components = relative_path.components().collect::<Vec<_>>();
    if components.is_empty() {
        return Err(migration_corrupt());
    }
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(migration_corrupt());
        };
        current.push(name);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(migration_io_error(error)),
        };
        let is_file = index + 1 == components.len();
        if metadata.file_type().is_symlink()
            || if is_file {
                !metadata.is_file() || metadata.len() > maximum_bytes
            } else {
                !metadata.is_dir()
            }
        {
            return Err(migration_corrupt());
        }
        let canonical = fs::canonicalize(&current).map_err(migration_io_error)?;
        if !canonical.starts_with(canonical_root) || !canonical.starts_with(session_root) {
            return Err(migration_corrupt());
        }
        if is_file {
            let file = fs::File::open(&current).map_err(migration_io_error)?;
            let opened = file.metadata().map_err(migration_io_error)?;
            if !opened.is_file() || opened.len() > maximum_bytes {
                return Err(migration_corrupt());
            }
            let mut bytes = Vec::with_capacity(opened.len() as usize);
            file.take(maximum_bytes.saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(migration_io_error)?;
            if bytes.len() as u64 > maximum_bytes {
                return Err(migration_corrupt());
            }
            *total_bytes = total_bytes
                .checked_add(bytes.len())
                .filter(|total| *total <= MAX_TOTAL_READ_BYTES)
                .ok_or_else(migration_corrupt)?;
            return Ok(Some(bytes));
        }
    }
    Err(migration_corrupt())
}

/// 将合法的旧全局或本会话 scoped loopback endpoint 归空；其它 authority/path 一律保留并报错。
///
/// @author kongweiguang
fn migrate_mcp_endpoint_context(bytes: &[u8], id: &AgentSessionId) -> AppResult<Option<Vec<u8>>> {
    let (mut document, header_changed) = parse_context_json(bytes, id)?;
    let object = document.as_object_mut().ok_or_else(migration_corrupt)?;
    if required_string(object, "transport")? != "streamable-http"
        || !required_bool(object, "toolsOnly")?
    {
        return Err(migration_corrupt());
    }
    match required_nullable_string(object, "endpoint")? {
        None if header_changed => encode_context_json(&document).map(Some),
        None => Ok(None),
        Some(endpoint) => {
            validate_legacy_mcp_endpoint(endpoint, id)?;
            object.insert("endpoint".to_owned(), Value::Null);
            encode_context_json(&document).map(Some)
        }
    }
}

/// 仅接受旧 server 实际生成的 127.0.0.1 HTTP 路径和有效端口，不猜测 localhost 或未知路由。
///
/// @author kongweiguang
fn validate_legacy_mcp_endpoint(endpoint: &str, id: &AgentSessionId) -> AppResult<()> {
    let tail = endpoint
        .strip_prefix("http://127.0.0.1:")
        .ok_or_else(migration_corrupt)?;
    let slash = tail.find('/').ok_or_else(migration_corrupt)?;
    let port = &tail[..slash];
    if port.is_empty()
        || !port.bytes().all(|byte| byte.is_ascii_digit())
        || !port.parse::<u16>().ok().is_some_and(|port| port != 0)
    {
        return Err(migration_corrupt());
    }
    let path = &tail[slash..];
    let scoped = format!("/mcp/agents/{id}");
    if path != "/mcp" && path != scoped {
        return Err(migration_corrupt());
    }
    Ok(())
}

/// 将旧 Ready target 快照降为 stale，并清除 Agent CLI runtime owner；保留目标证据与未知 JSON 字段。
///
/// @author kongweiguang
fn migrate_target_binding_context(bytes: &[u8], id: &AgentSessionId) -> AppResult<Option<Vec<u8>>> {
    let (mut document, header_changed) = parse_context_json(bytes, id)?;
    let object = document.as_object_mut().ok_or_else(migration_corrupt)?;
    let scope = object.get("scope").ok_or_else(migration_corrupt)?;
    let scope: AgentSessionScope =
        serde_json::from_value(scope.clone()).map_err(|_| migration_corrupt())?;
    if scope != AgentSessionScope::Global {
        return Err(migration_corrupt());
    }
    let binding_value = object.get_mut("binding").ok_or_else(migration_corrupt)?;
    let binding: TargetBindingContext =
        serde_json::from_value(binding_value.clone()).map_err(|_| migration_corrupt())?;
    if binding.status == AgentTargetLiveStatus::Ready
        && binding.target_terminal_session_id.is_none()
    {
        return Err(migration_corrupt());
    }
    binding.validate().map_err(|_| migration_corrupt())?;

    let binding_object = binding_value
        .as_object_mut()
        .ok_or_else(migration_corrupt)?;
    let mut changed = header_changed;
    if binding.status == AgentTargetLiveStatus::Ready {
        binding_object.insert("status".to_owned(), Value::String("stale".to_owned()));
        binding_object.insert("stale".to_owned(), Value::Bool(true));
        changed = true;
    }

    let agent_terminal = object
        .get_mut("agentTerminal")
        .ok_or_else(migration_corrupt)?;
    match agent_terminal {
        Value::Null => {}
        Value::Object(terminal) => {
            let is_stopped = match required_string(terminal, "status")? {
                "starting" | "running" | "closing" | "exited" | "closed" | "failed" => false,
                "stopped" => true,
                _ => return Err(migration_corrupt()),
            };
            let session_id = required_nullable_string(terminal, "sessionId")?;
            if !is_stopped && session_id.is_none() {
                return Err(migration_corrupt());
            }
            if let Some(session_id) = session_id {
                SessionId::parse(session_id).map_err(|_| migration_corrupt())?;
                terminal.insert("sessionId".to_owned(), Value::Null);
                changed = true;
            }
            if !is_stopped {
                terminal.insert("status".to_owned(), Value::String("stopped".to_owned()));
                changed = true;
            }
        }
        _ => return Err(migration_corrupt()),
    }
    if changed {
        encode_context_json(&document).map(Some)
    } else {
        Ok(None)
    }
}

/// 与投影文件一致地读取 camelCase binding 字段；未知字段由原 JSON Value 保留而不参与重写。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetBindingContext {
    #[serde(default)]
    binding_id: Option<String>,
    generation: u64,
    status: AgentTargetLiveStatus,
    stale: bool,
    #[serde(default)]
    pane_id: Option<PaneId>,
    #[serde(default)]
    tab_id: Option<TabId>,
    #[serde(default)]
    target_terminal_session_id: Option<SessionId>,
    #[serde(default)]
    target_ref: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    shell: Option<String>,
    #[serde(default)]
    target_kind: Option<String>,
}

impl TargetBindingContext {
    /// 使用领域 Ready 约束校验目标快照，避免迁移把缺失 id 伪装成可重新验证定义。
    ///
    /// @author kongweiguang
    fn validate(&self) -> AppResult<()> {
        if self.stale
            != matches!(
                self.status,
                AgentTargetLiveStatus::Stale | AgentTargetLiveStatus::Closed
            )
        {
            return Err(migration_corrupt());
        }
        AgentTargetBinding {
            binding_id: self.binding_id.clone(),
            generation: self.generation,
            pane_id: self.pane_id.clone(),
            tab_id: self.tab_id.clone(),
            terminal_session_id: self.target_terminal_session_id.clone(),
            target_ref: self.target_ref.clone(),
            target_kind: self.target_kind.clone(),
            cwd: self.cwd.clone(),
            shell: self.shell.clone(),
            live_status: self.status,
            last_seen_at: None,
        }
        .validate()
        .map_err(|_| migration_corrupt())
    }
}

/// 清除 terminal-snapshot 的静态目标引用，但保留历史输出及所有计数、脱敏和截断元数据。
///
/// @author kongweiguang
fn migrate_terminal_snapshot_context(
    bytes: &[u8],
    id: &AgentSessionId,
) -> AppResult<Option<Vec<u8>>> {
    let (mut document, header_changed) = parse_context_json(bytes, id)?;
    let object = document.as_object_mut().ok_or_else(migration_corrupt)?;
    let target_id = required_nullable_string(object, "targetTerminalSessionId")?;
    let captured_bytes = required_u64(object, "capturedBytes")?;
    let max_bytes = required_u64(object, "maxBytes")?;
    let output = required_string(object, "output")?;
    if captured_bytes != output.len() as u64 || captured_bytes > max_bytes {
        return Err(migration_corrupt());
    }
    let _truncated = required_bool(object, "truncated")?;
    let _redacted = required_bool(object, "redacted")?;
    if let Some(target_id) = target_id {
        SessionId::parse(target_id).map_err(|_| migration_corrupt())?;
        object.insert("targetTerminalSessionId".to_owned(), Value::Null);
        encode_context_json(&document).map(Some)
    } else if header_changed {
        encode_context_json(&document).map(Some)
    } else {
        Ok(None)
    }
}

/// 仅在迁移边界转换已确认的 snake_case envelope 键，再核对身份并保留其它 JSON 字段。
///
/// @author kongweiguang
fn parse_context_json(bytes: &[u8], id: &AgentSessionId) -> AppResult<(Value, bool)> {
    let mut document: Value = serde_json::from_slice(bytes).map_err(|_| migration_corrupt())?;
    let object = document.as_object_mut().ok_or_else(migration_corrupt)?;
    let header_changed = normalize_context_header_aliases(object)?;
    if required_u64(object, "schemaVersion")? != 1
        || required_string(object, "agentSessionId")? != id.as_str()
        || required_string(object, "generatedAt")?.trim().is_empty()
    {
        return Err(migration_corrupt());
    }
    Ok((document, header_changed))
}

/// 归一化旧运行态文档的两个身份头；同一语义双写时失败闭合，避免选择不确定值。
///
/// @author kongweiguang
fn normalize_context_header_aliases(object: &mut Map<String, Value>) -> AppResult<bool> {
    let aliases = [
        ("schema_version", "schemaVersion"),
        ("agent_session_id", "agentSessionId"),
    ];
    let mut changed = false;
    for (legacy, current) in aliases {
        if !object.contains_key(legacy) {
            continue;
        }
        if object.contains_key(current) {
            return Err(migration_corrupt());
        }
        let value = object.remove(legacy).ok_or_else(migration_corrupt)?;
        object.insert(current.to_owned(), value);
        changed = true;
    }
    Ok(changed)
}

/// 把有限 JSON 文档重新编码并保留尾换行；输入中未知字段值不会被投影丢弃。
///
/// @author kongweiguang
fn encode_context_json(document: &Value) -> AppResult<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(document).map_err(|error| {
        AppError::infrastructure_with_source("Agent context migration encoding failed", error)
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// 读取 JSON object 必填 string，不把缺失或 null 当作空值兼容。
///
/// @author kongweiguang
fn required_string<'a>(object: &'a Map<String, Value>, key: &str) -> AppResult<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(migration_corrupt)
}

/// 读取 JSON object 必填 unsigned integer，拒绝负数、浮点和缺失字段。
///
/// @author kongweiguang
fn required_u64(object: &Map<String, Value>, key: &str) -> AppResult<u64> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(migration_corrupt)
}

/// 读取 JSON object 必填 bool，保留 false 与缺失之间的 schema 区别。
///
/// @author kongweiguang
fn required_bool(object: &Map<String, Value>, key: &str) -> AppResult<bool> {
    object
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(migration_corrupt)
}

/// 读取 schema 中必填但允许 null 的 string，拒绝静默补写缺失目标字段。
///
/// @author kongweiguang
fn required_nullable_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> AppResult<Option<&'a str>> {
    match object.get(key) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        _ => Err(migration_corrupt()),
    }
}

/// 生成相对配置根的固定文件 edit，事务层可据 before 哈希拒绝外部并发修改。
///
/// @author kongweiguang
fn file_edit(
    id: &AgentSessionId,
    document_path: &Path,
    before: Vec<u8>,
    after: Vec<u8>,
) -> FileEdit {
    FileEdit {
        relative_path: PathBuf::from(SESSIONS_RELATIVE)
            .join(id.as_str())
            .join(document_path),
        before: Some(before),
        after,
    }
}

/// 为已验证的旧会话创建固定 provider 文件；before=None 让 core 用独占 CAS 拒绝并发新增。
///
/// @author kongweiguang
fn file_creation_edit(id: &AgentSessionId, document_path: &Path, after: Vec<u8>) -> FileEdit {
    FileEdit {
        relative_path: PathBuf::from(SESSIONS_RELATIVE)
            .join(id.as_str())
            .join(document_path),
        before: None,
        after,
    }
}

/// 生成与 AgentSession::mark_cli_stale 一致的 Unix 秒字符串；时钟异常不能伪造成功时间。
///
/// @author kongweiguang
fn unix_timestamp_seconds() -> AppResult<String> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            AppError::infrastructure_with_source("Agent migration clock failed", error)
        })?
        .as_secs();
    Ok(seconds.to_string())
}

/// 固定脱敏错误区域，避免把用户路径、文档正文或 provider 数据放进公开诊断。
///
/// @author kongweiguang
fn migration_corrupt() -> AppError {
    AppError::configuration_corrupt("Agent data migration")
}

/// 读取错误保留内部 source 供诊断，公开文案不展开文件路径和内容。
///
/// @author kongweiguang
fn migration_io_error(error: io::Error) -> AppError {
    AppError::infrastructure_with_source("Agent migration input could not be read", error)
}

#[cfg(test)]
#[path = "../../tests/support/agent_data_migration_unit.rs"]
mod tests;
