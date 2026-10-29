//! 文件化配置的单次格式迁移规划；仅读取源文件并返回可供 core 提交的字节编辑。
//!
//! @author kongweiguang

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use kerminal_application::{
    AppError, AppResult,
    connections::management::is_reserved_serial_host_tag,
    preferences::{KeybindingPreference, SettingsSnapshot},
};
use kerminal_domain::preferences::{ConfigDocumentPath, ConfigurationIssueKind, ConfigurationMode};
use serde::Deserialize;
use toml_edit::{DocumentMut, InlineTable, Item, Key, Table, Value};

use crate::configuration;

const HOST_SCHEMA_V1: i64 = 1;
const HOST_SCHEMA_V2: i64 = 2;
const HOST_SCHEMA_V3: i64 = 3;
const DOCUMENT_SCHEMA_V1: i64 = 1;
const CONFIG_GUIDE_REVISION_1_SHA256: &str =
    "75b27c557eca803376f4b710a56e44a4d8ae88880973fded921a5f9ff22edbb9";
const CONFIG_GUIDE_REVISION_2_SHA256: &str =
    "48c8c6de62ac3f91b6e1ca47b92e3aa35c8d19582ebd5d5c9487a0b2edf783f4";
const CONFIG_GUIDE_REVISION_3_SHA256: &str =
    "0953e4af8bea80af32b30651b276a438d4c304ee0ad56f8b1bc9654fea57a60b";
const CONFIG_GUIDE_REVISION_5_SHA256: &str =
    "1d9905e04ff62547b84449a7682d921b450f19ab09193d28488de3171a734841";
const CONFIG_GUIDE_REVISION_6_SHA256: &str =
    "b48806571ccf54414d256ecba3d4eed6e652052366ef0ba9ffa1f67611227baf";
const CONFIG_GUIDE_REVISION_7_SHA256: &str =
    "13f5d92dbd66032b2f3fcbe18230f1169717d6a80e7b5acbbda38419fa9fd541";
const CONFIG_GUIDE_REVISION_8_SHA256: &str =
    "9daf04d7412a04285e8be3d708cb6b0b7e5e0842321dabfc5c6567a96f1535c7";
const CONFIG_GUIDE_REVISION_9_SHA256: &str =
    "41a2d9a34f503a5182027880595f2d42ebb7181b6120e1744fbfa3e93ff1382c";
const CONFIG_GUIDE_REVISION_10_SHA256: &str =
    "721c2d1853c54b2c1beed73e513989545de4f2afb28bf00a7c239e68958d17f7";
const LEGACY_TAURI_CONFIG_GUIDE_SHA256: &str =
    "e70de091c515a0a143cd04f379a4274fbe18f810a411a6ca273277bc743220ad";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsKeybindingMigrationView {
    keybindings: Vec<KeybindingPreference>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DocumentKind {
    Settings,
    Profile,
    HostGroups,
    Host,
    Snippet,
    Workflow,
}

struct DocumentSource {
    relative_path: PathBuf,
    document_path: ConfigDocumentPath,
    kind: DocumentKind,
    bytes: Vec<u8>,
}

/// 在任何写事务前完成当前文档校验、旧格式转换和输出校验；不创建目录或初始化指南。
///
/// @author kongweiguang
pub(super) fn plan_configuration_migration(root: &Path) -> AppResult<Vec<super::FileEdit>> {
    let mut sources = collect_configuration_sources(root)?;
    sources.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    ensure_recovery_state_is_migratable(root, &sources)?;

    let mut edits = Vec::new();
    for source in sources {
        if let Some(edit) = plan_document_edit(source)? {
            if path_contains_symlink(root, &edit.relative_path)? {
                return Err(AppError::conflict(
                    "需要迁移的配置路径经过符号链接，迁移已拒绝",
                ));
            }
            edits.push(edit);
        }
    }
    plan_config_guide_edit(root, &mut edits)?;
    edits.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(edits)
}

/// 只枚举应用拥有的 TOML 路径；未知目录和本机状态由其他迁移边界处理。
///
/// @author kongweiguang
fn collect_configuration_sources(root: &Path) -> AppResult<Vec<DocumentSource>> {
    let mut sources = Vec::new();
    push_fixed_source(
        root,
        PathBuf::from("settings.toml"),
        ConfigDocumentPath::Settings,
        DocumentKind::Settings,
        &mut sources,
    )?;
    push_fixed_source(
        root,
        PathBuf::from("hosts/groups.toml"),
        ConfigDocumentPath::HostGroups,
        DocumentKind::HostGroups,
        &mut sources,
    )?;

    for (directory, kind) in [
        ("profiles", DocumentKind::Profile),
        ("hosts", DocumentKind::Host),
        ("snippets", DocumentKind::Snippet),
        ("workflows", DocumentKind::Workflow),
    ] {
        let absolute_directory = root.join(directory);
        let metadata = match fs::metadata(&absolute_directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(storage_error("读取配置目录失败", error)),
        };
        if !metadata.is_dir() {
            return Err(AppError::invalid("配置目录不是可读取目录"));
        }
        let entries = fs::read_dir(&absolute_directory)
            .map_err(|error| storage_error("读取配置目录失败", error))?;
        let mut entries = entries
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("读取配置文件列表失败", error))?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let path = entry.path();
            if path.extension() != Some(OsStr::new("toml")) {
                continue;
            }
            if kind == DocumentKind::Host && entry.file_name() == OsStr::new("groups.toml") {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|error| storage_error("检查配置文件类型失败", error))?;
            if !file_type.is_file() && !file_type.is_symlink() {
                return Err(AppError::invalid("配置路径不是普通文件"));
            }
            let file_name = entry
                .file_name()
                .into_string()
                .map_err(|_| AppError::invalid("配置文件名不是有效文本"))?;
            let id = Path::new(&file_name)
                .file_stem()
                .and_then(OsStr::to_str)
                .ok_or_else(|| AppError::invalid("配置文件名缺少有效 ID"))?;
            let document_path = document_path_for_id(kind, id)?;
            let relative_path = PathBuf::from(directory).join(&file_name);
            if document_path
                .relative_path()
                .map_err(|_| AppError::invalid("配置文件 ID 无效"))?
                != relative_path
            {
                return Err(AppError::invalid("配置文件名与受支持路径不匹配"));
            }
            let bytes = read_required_file(root, &relative_path)?;
            sources.push(DocumentSource {
                relative_path,
                document_path,
                kind,
                bytes,
            });
        }
    }
    Ok(sources)
}

/// 读取固定路径文档；文件缺失代表尚未创建，不自动播种默认数据。
///
/// @author kongweiguang
fn push_fixed_source(
    root: &Path,
    relative_path: PathBuf,
    document_path: ConfigDocumentPath,
    kind: DocumentKind,
    sources: &mut Vec<DocumentSource>,
) -> AppResult<()> {
    if let Some(bytes) = read_optional_file(root, &relative_path)? {
        sources.push(DocumentSource {
            relative_path,
            document_path,
            kind,
            bytes,
        });
    }
    Ok(())
}

/// 为目录型配置文件构造受限领域路径，避免迁移从任意文件名推导输出位置。
///
/// @author kongweiguang
fn document_path_for_id(kind: DocumentKind, id: &str) -> AppResult<ConfigDocumentPath> {
    let path = match kind {
        DocumentKind::Profile => ConfigDocumentPath::Profile(id.to_owned()),
        DocumentKind::Host => ConfigDocumentPath::Host(id.to_owned()),
        DocumentKind::Snippet => ConfigDocumentPath::Snippet(id.to_owned()),
        DocumentKind::Workflow => ConfigDocumentPath::Workflow(id.to_owned()),
        DocumentKind::Settings | DocumentKind::HostGroups => {
            return Err(AppError::invalid("固定配置路径不能使用目录型 ID"));
        }
    };
    path.relative_path()
        .map_err(|_| AppError::invalid("配置文件 ID 无效"))?;
    Ok(path)
}

/// 检查目标和文件类型后读取原始字节；坏链接和非普通文件都阻断迁移。
///
/// @author kongweiguang
fn read_optional_file(root: &Path, relative_path: &Path) -> AppResult<Option<Vec<u8>>> {
    let absolute_path = root.join(relative_path);
    let link_metadata = match fs::symlink_metadata(&absolute_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(storage_error("读取配置文件元数据失败", error)),
    };
    let metadata = if link_metadata.file_type().is_symlink() {
        fs::metadata(&absolute_path)
            .map_err(|error| storage_error("读取配置链接目标失败", error))?
    } else {
        link_metadata
    };
    if !metadata.is_file() {
        return Err(AppError::invalid("配置路径不是普通文件"));
    }
    fs::read(absolute_path)
        .map(Some)
        .map_err(|error| storage_error("读取配置文件失败", error))
}

/// 固定路径只读迁移与目录枚举共用同一文件读取边界。
///
/// @author kongweiguang
fn read_required_file(root: &Path, relative_path: &Path) -> AppResult<Vec<u8>> {
    read_optional_file(root, relative_path)?
        .ok_or_else(|| AppError::conflict("配置文件在迁移预检期间消失"))
}

/// 只在文件确实需要改写时阻断链接路径，避免提交阶段替换链接或写入根外目录。
///
/// @author kongweiguang
fn path_contains_symlink(root: &Path, relative_path: &Path) -> AppResult<bool> {
    let mut absolute = root.to_path_buf();
    for component in relative_path.components() {
        let Component::Normal(component) = component else {
            return Err(AppError::invalid("迁移目标不是安全相对路径"));
        };
        absolute.push(component);
        match fs::symlink_metadata(&absolute) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(AppError::conflict("配置路径在迁移预检期间消失"));
            }
            Err(error) => return Err(storage_error("检查配置路径链接失败", error)),
        }
    }
    Ok(false)
}

/// 只允许旧 host schema 与 snippet canonical rename 可解释的预检诊断；输出仍逐文档校验。
///
/// @author kongweiguang
fn ensure_recovery_state_is_migratable(root: &Path, sources: &[DocumentSource]) -> AppResult<()> {
    let recovery = configuration::inspect_current_configuration(root);
    if recovery.mode == ConfigurationMode::ReadWrite {
        return Ok(());
    }
    for issue in recovery.issues {
        let has_migratable_source = sources.iter().any(|source| {
            if normalized_path(&source.relative_path) != issue.relative_path {
                return false;
            }
            matches!(
                (
                    source.kind,
                    source_schema_version(&source.bytes),
                    issue.kind
                ),
                (
                    DocumentKind::Host,
                    Some(HOST_SCHEMA_V1 | HOST_SCHEMA_V2),
                    ConfigurationIssueKind::UnsupportedSchema,
                ) | (
                    DocumentKind::Snippet,
                    Some(DOCUMENT_SCHEMA_V1),
                    ConfigurationIssueKind::MalformedToml,
                )
            )
        });
        if !has_migratable_source {
            return Err(AppError::conflict(
                "配置根包含无效或不支持的文档，迁移未执行",
            ));
        }
    }
    Ok(())
}

/// 在只读源上规划一份配置文件编辑，并用当前 validator 检查输出文档。
///
/// @author kongweiguang
fn plan_document_edit(source: DocumentSource) -> AppResult<Option<super::FileEdit>> {
    let original = std::str::from_utf8(&source.bytes)
        .map_err(|error| storage_error("配置文件不是有效 UTF-8", error))?;
    let mut document = original
        .parse::<DocumentMut>()
        .map_err(|error| storage_error("配置文件不是有效 TOML", error))?;
    let schema_version = document
        .get("schema_version")
        .and_then(Item::as_integer)
        .ok_or_else(|| AppError::invalid("现有配置文件缺少有效 schema_version"))?;

    let migrated = match source.kind {
        DocumentKind::Host => migrate_host_document(&mut document, schema_version)?,
        DocumentKind::Settings => {
            require_document_schema(schema_version)?;
            let mut migrated = migrate_settings_keybindings(&mut document)?;
            if let Some(sftp) = document.get_mut("sftp") {
                migrated |= normalize_alias(
                    sftp,
                    "timeoutSeconds",
                    "idleTimeoutSeconds",
                    &source.relative_path,
                )?;
            }
            migrated
        }
        DocumentKind::Snippet => {
            require_document_schema(schema_version)?;
            let mut migrated = false;
            for (section, old, current) in [
                ("variables", "defaultValue", "default_value"),
                ("variables", "renderStrategy", "render_strategy"),
                ("context_bindings", "targetId", "target_id"),
            ] {
                if let Some(item) = document.get_mut(section) {
                    migrated |= normalize_alias(item, old, current, &source.relative_path)?;
                }
            }
            if let Some(variables) = document.get_mut("variables") {
                migrated |= migrate_missing_snippet_variable_fields(variables);
            }
            migrated
        }
        DocumentKind::Profile | DocumentKind::HostGroups | DocumentKind::Workflow => {
            require_document_schema(schema_version)?;
            false
        }
    };

    validate_document_identity(&source.document_path, &document)?;
    if !migrated {
        configuration::validate_current_configuration_document(&source.document_path, original)?;
        return Ok(None);
    }

    let replacement = document.to_string();
    configuration::validate_current_configuration_document(&source.document_path, &replacement)?;
    let after = replacement.into_bytes();
    if after == source.bytes {
        return Ok(None);
    }
    Ok(Some(super::FileEdit {
        relative_path: source.relative_path,
        before: Some(source.bytes),
        after,
    }))
}

/// 保持现有文件 ID 与其受限路径一致，避免 schema 转换后仍无法由当前仓储读取。
///
/// @author kongweiguang
fn validate_document_identity(path: &ConfigDocumentPath, document: &DocumentMut) -> AppResult<()> {
    let expected_id = match path {
        ConfigDocumentPath::Profile(id)
        | ConfigDocumentPath::Host(id)
        | ConfigDocumentPath::Snippet(id)
        | ConfigDocumentPath::Workflow(id) => Some(id.as_str()),
        ConfigDocumentPath::Settings | ConfigDocumentPath::HostGroups => None,
    };
    if expected_id
        .is_some_and(|expected| document.get("id").and_then(Item::as_str) != Some(expected))
    {
        return Err(AppError::invalid("配置文档 ID 与文件名不一致"));
    }
    Ok(())
}

/// 只允许普通配置 schema 1；缺失 marker 的既有文件不是可猜测的旧版本。
///
/// @author kongweiguang
fn require_document_schema(schema_version: i64) -> AppResult<()> {
    if schema_version == DOCUMENT_SCHEMA_V1 {
        Ok(())
    } else {
        Err(AppError::invalid("普通配置 schema_version 不受支持"))
    }
}

/// 只对旧文档中显式存在的非空快捷键列表补动作，保留空列表作为用户选择。
///
/// @author kongweiguang
fn migrate_settings_keybindings(document: &mut DocumentMut) -> AppResult<bool> {
    if document.get("keybindings").is_none() {
        return Ok(false);
    }
    let parsed = toml_edit::de::from_document::<SettingsKeybindingMigrationView>(document.clone())
        .map_err(|source| AppError::configuration_corrupt("设置").with_source(source))?;
    let existing_count = parsed.keybindings.len();
    if existing_count == 0 {
        return Ok(false);
    }

    let completed = complete_keybinding_actions_for_upgrade(parsed.keybindings);
    if completed.len() == existing_count {
        return Ok(false);
    }
    let additions = completed.into_iter().skip(existing_count);
    let keybindings = document
        .get_mut("keybindings")
        .ok_or_else(|| AppError::conflict("快捷键配置在迁移预检期间消失"))?;
    for binding in additions {
        append_keybinding_default(keybindings, &binding)?;
    }
    Ok(true)
}

/// 仅在一次性升级中为既有非空快捷键表补齐当前默认动作；日常设置读取不调用此规则。
///
/// @author kongweiguang
fn complete_keybinding_actions_for_upgrade(
    mut keybindings: Vec<KeybindingPreference>,
) -> Vec<KeybindingPreference> {
    if keybindings.is_empty() {
        return keybindings;
    }

    let defaults = SettingsSnapshot::default().keybindings;
    for action in ["application.quit", "terminal.paste"] {
        if keybindings.iter().any(|binding| binding.action == action) {
            continue;
        }
        let default = defaults
            .iter()
            .find(|binding| binding.action == action)
            .cloned();
        let Some(default) = default else {
            continue;
        };
        let default = if action == "terminal.paste" {
            paste_default_preserving_existing_bindings(&keybindings, default)
        } else {
            default
        };
        keybindings.push(default);
    }
    keybindings
}

/// 为升级新增的粘贴键位避开现有平台绑定，保留当前明确的冲突处理策略。
///
/// @author kongweiguang
fn paste_default_preserving_existing_bindings(
    existing: &[KeybindingPreference],
    mut paste: KeybindingPreference,
) -> KeybindingPreference {
    if binding_exists_on_platform(
        existing,
        KeybindingPlatform::Windows,
        &paste.windows_binding,
    ) {
        paste.windows_binding.clear();
    }
    if let Some(alternate) = paste.windows_alternate_binding.as_mut()
        && binding_exists_on_platform(existing, KeybindingPlatform::Windows, alternate)
    {
        alternate.clear();
    }
    if let Some(linux) = paste.linux_binding.as_mut()
        && binding_exists_on_platform(existing, KeybindingPlatform::Linux, linux)
    {
        linux.clear();
    }
    if binding_exists_on_platform(existing, KeybindingPlatform::Mac, &paste.mac_binding) {
        paste.mac_binding.clear();
    }
    paste
}

/// 比较新增默认键与现有平台键，Linux 的 None 继续采用当前的跨平台继承规则。
///
/// @author kongweiguang
fn binding_exists_on_platform(
    existing: &[KeybindingPreference],
    platform: KeybindingPlatform,
    candidate: &str,
) -> bool {
    let Some(candidate) = first_canonical_stroke(candidate) else {
        return false;
    };
    existing.iter().any(|preference| {
        let binding = match platform {
            KeybindingPlatform::Windows => {
                fallback_binding(&preference.windows_binding, &preference.binding)
            }
            KeybindingPlatform::Linux => preference.linux_binding.as_deref().unwrap_or_else(|| {
                fallback_binding(&preference.windows_binding, &preference.binding)
            }),
            KeybindingPlatform::Mac => {
                fallback_binding(&preference.mac_binding, &preference.binding)
            }
        };
        let primary_conflicts =
            first_canonical_stroke(binding).as_deref() == Some(candidate.as_str());
        let alternate_conflicts = platform == KeybindingPlatform::Windows
            && preference
                .windows_alternate_binding
                .as_deref()
                .and_then(first_canonical_stroke)
                .as_deref()
                == Some(candidate.as_str());
        primary_conflicts || alternate_conflicts
    })
}

/// 旧设置的空平台字段仍回退到通用绑定，不因迁移改变其现行读取语义。
///
/// @author kongweiguang
fn fallback_binding<'a>(platform_binding: &'a str, generic_binding: &'a str) -> &'a str {
    if platform_binding.trim().is_empty() {
        generic_binding
    } else {
        platform_binding
    }
}

/// 将冲突比较中的常用修饰键别名归一化，不承担快捷键解析或运行时分派。
///
/// @author kongweiguang
fn canonical_shortcut(binding: &str) -> Option<String> {
    let binding = binding.trim();
    if binding.is_empty() {
        return None;
    }
    let mut strokes = Vec::new();
    for stroke in binding.split_whitespace() {
        let mut parts = stroke
            .split(['+', '-'])
            .filter(|part| !part.trim().is_empty())
            .map(|part| match part.trim().to_ascii_lowercase().as_str() {
                "control" => "ctrl".to_owned(),
                "command" | "meta" => "cmd".to_owned(),
                "option" => "alt".to_owned(),
                value if value.starts_with("key") && value.len() == 4 => value[3..].to_owned(),
                value if value.starts_with("digit") && value.len() == 6 => value[5..].to_owned(),
                value => value.to_owned(),
            })
            .collect::<Vec<_>>();
        parts.sort_unstable();
        strokes.push(parts.join("+"));
    }
    (!strokes.is_empty()).then(|| strokes.join(" "))
}

/// 使用 chord 的首个按键检测冲突，避免新增单键抢占用户原组合输入。
///
/// @author kongweiguang
fn first_canonical_stroke(binding: &str) -> Option<String> {
    canonical_shortcut(binding)?
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeybindingPlatform {
    Windows,
    Linux,
    Mac,
}

/// 用当前应用的动作目录生成新增行，并沿用源文件使用的 TOML 数组形状。
///
/// @author kongweiguang
fn append_keybinding_default(
    keybindings: &mut Item,
    binding: &KeybindingPreference,
) -> AppResult<()> {
    let serialized = toml_edit::ser::to_document(binding)
        .map_err(|source| AppError::configuration_corrupt("设置").with_source(source))?;
    let table = serialized.as_table().clone();
    match keybindings {
        Item::Value(Value::Array(rows)) => {
            rows.push(Value::InlineTable(table.into_inline_table()));
            Ok(())
        }
        Item::ArrayOfTables(rows) => {
            rows.push(table);
            Ok(())
        }
        _ => Err(AppError::invalid("快捷键配置不是受支持的数组")),
    }
}

/// 在一次性迁移边界为旧变量显式写入原 reader 的安全缺省，当前 reader 不再猜测；完整遍历保留所有行的迁移副作用。
///
/// @author kongweiguang
fn migrate_missing_snippet_variable_fields(variables: &mut Item) -> bool {
    const DEFAULTS: [(&str, &str); 2] = [("kind", "text"), ("render_strategy", "shellArg")];
    match variables {
        Item::Table(table) => add_missing_table_defaults(table, &DEFAULTS),
        Item::ArrayOfTables(tables) => {
            let mut changed = false;
            for table in tables.iter_mut() {
                changed |= add_missing_table_defaults(table, &DEFAULTS);
            }
            changed
        }
        Item::Value(Value::InlineTable(table)) => add_missing_inline_defaults(table, &DEFAULTS),
        Item::Value(Value::Array(values)) => {
            let mut changed = false;
            for value in values.iter_mut() {
                if let Some(table) = value.as_inline_table_mut() {
                    changed |= add_missing_inline_defaults(table, &DEFAULTS);
                }
            }
            changed
        }
        _ => false,
    }
}

/// 仅给普通变量表中缺少的字段设旧语义值，任何已存在的用户值都保持原样。
///
/// @author kongweiguang
fn add_missing_table_defaults(table: &mut Table, defaults: &[(&str, &str)]) -> bool {
    let mut changed = false;
    for (key, value) in defaults {
        if !table.contains_key(key) {
            table.insert(key, toml_edit::value(*value));
            changed = true;
        }
    }
    changed
}

/// 与普通表使用相同的缺项规则，适配当前预置和用户可编辑的内联变量数组。
///
/// @author kongweiguang
fn add_missing_inline_defaults(table: &mut InlineTable, defaults: &[(&str, &str)]) -> bool {
    let mut changed = false;
    for (key, value) in defaults {
        if !table.contains_key(key) {
            table.insert(*key, Value::from(*value));
            changed = true;
        }
    }
    changed
}

/// host schema 与选项契约分别升级；已知旧参数只在显式迁移入口转换，业务 reader 不容错。
///
/// @author kongweiguang
fn migrate_host_document(document: &mut DocumentMut, schema_version: i64) -> AppResult<bool> {
    match schema_version {
        HOST_SCHEMA_V3 => return migrate_host_connection_options(document),
        HOST_SCHEMA_V1 => {
            if document.get("protocol").is_some() {
                return Err(AppError::conflict(
                    "host schema v1 含有 protocol，属于半迁移状态",
                ));
            }
            let protocol = legacy_protocol(document);
            replace_host_schema_version(document)?;
            document
                .as_table_mut()
                .insert("protocol", toml_edit::value(protocol));
        }
        HOST_SCHEMA_V2 => replace_host_schema_version(document)?,
        _ => return Err(AppError::invalid("host schema_version 不受支持")),
    }

    migrate_legacy_serial_fields(document)?;
    if document
        .get("production")
        .is_some_and(|item| item.as_value().and_then(Value::as_bool).is_none())
    {
        return Err(AppError::invalid("host 的退役 production 字段类型无效"));
    }
    document.as_table_mut().remove("production");
    migrate_host_connection_options(document)?;
    Ok(true)
}

/// 对照旧持久 SshOptions 的 camelCase 字段转换，未实现的旧扩展保留原文而不伪造支持。
fn migrate_host_connection_options(document: &mut DocumentMut) -> AppResult<bool> {
    if !matches!(
        document.get("protocol").and_then(Item::as_str),
        Some("ssh" | "sftp")
    ) {
        return Ok(false);
    }
    let Some(ssh_options) = document.get_mut("ssh_options") else {
        return Ok(false);
    };
    let ssh = ssh_options
        .as_table_like_mut()
        .ok_or_else(|| AppError::invalid("旧 SSH 选项不是受支持的表"))?;
    let mut changed = false;
    if let Some(terminal) = ssh.get_mut("terminal") {
        for (old, current) in [
            ("terminalType", "term"),
            ("connectTimeoutSeconds", "connect_timeout_seconds"),
            ("keepaliveSeconds", "keepalive_seconds"),
        ] {
            changed |= normalize_alias(terminal, old, current, Path::new("host-options"))?;
        }
    }
    let Some(mut transfer) = ssh.remove("transfer") else {
        return Ok(changed);
    };
    for (old, current) in [
        ("localStartDirectory", "local_start_directory"),
        ("remoteStartDirectory", "remote_start_directory"),
        ("preserveTimestamps", "preserve_mtime"),
        ("followSymlinks", "follow_source_symlinks"),
        ("maxConcurrentTransfers", "concurrency"),
    ] {
        normalize_alias(&mut transfer, old, current, Path::new("host-options"))?;
    }
    let legacy_table = transfer
        .as_table_like_mut()
        .ok_or_else(|| AppError::invalid("旧 SFTP 选项不是受支持的表"))?;
    // 旧 reader 的空目录表示继承，不把它转换成当前模型中的显式空路径。
    for key in ["local_start_directory", "remote_start_directory"] {
        if legacy_table
            .get(key)
            .and_then(Item::as_str)
            .is_some_and(|path| path.trim().is_empty())
        {
            legacy_table.remove(key);
        }
    }
    if let Some(current) = ssh.get_mut("sftp") {
        let current = current
            .as_table_like_mut()
            .ok_or_else(|| AppError::invalid("当前 SFTP 选项不是受支持的表"))?;
        let entries = legacy_table
            .iter()
            .map(|(key, value)| (key.to_owned(), value.clone()))
            .collect::<Vec<_>>();
        for (key, value) in entries {
            if let Some(existing) = current.get(&key) {
                if !same_scalar_value(existing, &value) {
                    return Err(alias_conflict(Path::new("host-options")));
                }
            } else {
                current.insert(&key, value);
            }
        }
    } else {
        ssh.insert("sftp", transfer);
    }
    Ok(true)
}

/// 只在 v1/v2→v3 边界迁移串口 tag；先核验必需旧 endpoint，避免 tag 掩盖损坏字段。
///
/// @author kongweiguang
fn migrate_legacy_serial_fields(document: &mut DocumentMut) -> AppResult<()> {
    let protocol = document
        .get("protocol")
        .and_then(Item::as_str)
        .ok_or_else(|| AppError::invalid("旧 host 缺少有效 protocol"))?;
    let serial_tags = parse_legacy_serial_tags(document)?;
    if protocol != "serial" {
        if serial_tags.has_reserved_tag || document.get("serial").is_some() {
            return Err(AppError::invalid("非串口 host 含有串口专属字段"));
        }
        return Ok(());
    }

    validate_legacy_serial_endpoint(document)?;

    if document.get("serial").is_some() {
        return Err(AppError::invalid("旧 host 已包含未支持的 serial 表"));
    }
    let device = serial_tags
        .device
        .or_else(|| {
            document
                .get("host")
                .and_then(Item::as_str)
                .map(str::to_owned)
        })
        .ok_or_else(|| AppError::invalid("旧串口 host 缺少设备路径"))?;
    let device = device.trim();
    if device.is_empty() || device.chars().any(char::is_control) {
        return Err(AppError::invalid("旧串口设备路径无效"));
    }

    let root = document.as_table_mut();
    let serial = root
        .entry("serial")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| AppError::invalid("旧 host 的 serial 字段结构无效"))?;
    serial.insert("device", toml_edit::value(device));
    serial.insert(
        "baud_rate",
        toml_edit::value(i64::from(serial_tags.baud_rate.unwrap_or(9_600))),
    );
    serial.insert(
        "data_bits",
        toml_edit::value(i64::from(serial_tags.data_bits.unwrap_or(8))),
    );
    serial.insert(
        "stop_bits",
        toml_edit::value(i64::from(serial_tags.stop_bits.unwrap_or(1))),
    );
    serial.insert(
        "parity",
        toml_edit::value(serial_tags.parity.unwrap_or("none")),
    );
    serial.insert(
        "flow_control",
        toml_edit::value(serial_tags.flow_control.unwrap_or("none")),
    );
    root.remove("host");
    root.remove("port");
    root.remove("username");
    remove_legacy_serial_tags(root)?;
    Ok(())
}

/// 验证旧串口文档被移除的必需 endpoint 字段，范围遵循旧 schema 的 u16 端口契约。
///
/// @author kongweiguang
fn validate_legacy_serial_endpoint(document: &DocumentMut) -> AppResult<()> {
    if !document
        .get("host")
        .is_some_and(|item| item.as_str().is_some())
    {
        return Err(AppError::invalid("旧串口 host 字段缺失或类型无效"));
    }
    let port = document.get("port").and_then(Item::as_integer);
    if !port.is_some_and(|port| (0..=u16::MAX as i64).contains(&port)) {
        return Err(AppError::invalid("旧串口 port 字段缺失或超出范围"));
    }
    if !document
        .get("username")
        .is_some_and(|item| item.as_str().is_some())
    {
        return Err(AppError::invalid("旧串口 username 字段缺失或类型无效"));
    }
    Ok(())
}

#[derive(Default)]
struct LegacySerialTags {
    device: Option<String>,
    baud_rate: Option<u32>,
    data_bits: Option<u8>,
    stop_bits: Option<u8>,
    parity: Option<&'static str>,
    flow_control: Option<&'static str>,
    has_reserved_tag: bool,
}

/// 唯一解析旧格式的串口 tags，重复字段、短值错误和歧义值一律阻断迁移。
///
/// @author kongweiguang
fn parse_legacy_serial_tags(document: &DocumentMut) -> AppResult<LegacySerialTags> {
    let Some(item) = document.get("tags") else {
        return Ok(LegacySerialTags::default());
    };
    let tags = item
        .as_array()
        .ok_or_else(|| AppError::invalid("旧 host tags 必须是字符串数组"))?;
    let mut parsed = LegacySerialTags::default();
    let mut seen_marker = false;
    for tag in tags.iter() {
        let tag = tag
            .as_str()
            .ok_or_else(|| AppError::invalid("旧 host tags 只能包含字符串"))?;
        let tag = tag.trim();
        let (key, value) = tag
            .split_once(':')
            .map_or((tag, None), |(key, value)| (key.trim(), Some(value.trim())));
        let key = key.to_ascii_lowercase();
        let Some(value) = value else {
            if key == "serial" {
                if seen_marker {
                    return Err(AppError::invalid("旧 host 重复声明 serial tag"));
                }
                seen_marker = true;
                parsed.has_reserved_tag = true;
            } else if is_reserved_serial_host_tag(&key) {
                return Err(AppError::invalid("旧 host 串口 tag 缺少值"));
            }
            continue;
        };
        let reserved = match key.as_str() {
            "serial" => {
                return Err(AppError::invalid("旧 host serial 标记不能包含值"));
            }
            "serial-port" => {
                set_legacy_once(
                    &mut parsed.device,
                    value.to_owned(),
                    "旧 host 重复声明 serial-port",
                )?;
                true
            }
            "serial-baud" => {
                let number = value
                    .parse::<u32>()
                    .ok()
                    .filter(|number| (300..=4_000_000).contains(number))
                    .ok_or_else(|| AppError::invalid("旧 host 串口波特率无效"))?;
                set_legacy_once(
                    &mut parsed.baud_rate,
                    number,
                    "旧 host 重复声明 serial-baud",
                )?;
                true
            }
            "serial-data-bits" => {
                let number = value
                    .parse::<u8>()
                    .ok()
                    .filter(|number| (5..=8).contains(number))
                    .ok_or_else(|| AppError::invalid("旧 host 串口数据位无效"))?;
                set_legacy_once(
                    &mut parsed.data_bits,
                    number,
                    "旧 host 重复声明 serial-data-bits",
                )?;
                true
            }
            "serial-stop-bits" => {
                let number = value
                    .parse::<u8>()
                    .ok()
                    .filter(|number| matches!(number, 1 | 2))
                    .ok_or_else(|| AppError::invalid("旧 host 串口停止位无效"))?;
                set_legacy_once(
                    &mut parsed.stop_bits,
                    number,
                    "旧 host 重复声明 serial-stop-bits",
                )?;
                true
            }
            "serial-parity" => {
                let canonical = match value.to_ascii_lowercase().as_str() {
                    "none" | "n" => "none",
                    "even" | "e" => "even",
                    "odd" | "o" => "odd",
                    _ => return Err(AppError::invalid("旧 host 串口 parity 无效")),
                };
                set_legacy_once(
                    &mut parsed.parity,
                    canonical,
                    "旧 host 重复声明 serial-parity",
                )?;
                true
            }
            "serial-flow" => {
                let canonical = match value.to_ascii_lowercase().as_str() {
                    "none" | "n" => "none",
                    "xonxoff" | "x" => "xonxoff",
                    "rtscts" | "r" => "rtscts",
                    _ => return Err(AppError::invalid("旧 host 串口 flow control 无效")),
                };
                set_legacy_once(
                    &mut parsed.flow_control,
                    canonical,
                    "旧 host 重复声明 serial-flow",
                )?;
                true
            }
            _ => false,
        };
        parsed.has_reserved_tag |= reserved;
    }
    Ok(parsed)
}

/// 串口 tag 字段只保留唯一值，调用方传固定诊断文案以符合错误类型的静态约束。
///
/// @author kongweiguang
fn set_legacy_once<T>(slot: &mut Option<T>, value: T, message: &'static str) -> AppResult<()> {
    if slot.replace(value).is_some() {
        return Err(AppError::invalid(message));
    }
    Ok(())
}

/// 只从 tags 数组移除已转换的串口字段，保留普通标签及每个 TOML 值的格式装饰。
///
/// @author kongweiguang
fn remove_legacy_serial_tags(root: &mut Table) -> AppResult<()> {
    let Some(tags) = root.get_mut("tags") else {
        return Ok(());
    };
    let tags = tags
        .as_array_mut()
        .ok_or_else(|| AppError::invalid("旧 host tags 必须是字符串数组"))?;
    tags.retain(|tag| {
        tag.as_str().is_none_or(|tag| {
            let key = tag
                .split_once(':')
                .map_or(tag, |(key, _)| key)
                .trim()
                .to_ascii_lowercase();
            !is_reserved_serial_host_tag(&key)
        })
    });
    Ok(())
}

/// 由旧 tags 推导连接协议，保留历史优先级并将默认协议固定为 SSH。
///
/// @author kongweiguang
fn legacy_protocol(document: &DocumentMut) -> &'static str {
    let tags = document
        .get("tags")
        .and_then(Item::as_array)
        .into_iter()
        .flat_map(|tags| tags.iter())
        .filter_map(Value::as_str)
        .map(|tag| tag.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();
    if tags.iter().any(|tag| tag == "serial") {
        "serial"
    } else if tags.iter().any(|tag| tag == "telnet") {
        "telnet"
    } else if tags.iter().any(|tag| tag == "rdp") {
        "rdp"
    } else {
        "ssh"
    }
}

/// 替换 host schema 时保留原整数 token 的注释和空白装饰。
///
/// @author kongweiguang
fn replace_host_schema_version(document: &mut DocumentMut) -> AppResult<()> {
    let item = document
        .get_mut("schema_version")
        .and_then(Item::as_value_mut)
        .ok_or_else(|| AppError::invalid("host schema_version 格式无效"))?;
    let mut replacement = Value::from(HOST_SCHEMA_V3);
    *replacement.decor_mut() = item.decor().clone();
    *item = replacement;
    Ok(())
}

/// 将指定别名改为 canonical key；冲突值不猜测，单字段移动保留原 TOML 装饰。
///
/// @author kongweiguang
fn normalize_alias(
    item: &mut Item,
    legacy_key: &str,
    canonical_key: &str,
    relative_path: &Path,
) -> AppResult<bool> {
    match item {
        Item::Table(table) => {
            normalize_table_alias(table, legacy_key, canonical_key, relative_path)
        }
        Item::ArrayOfTables(tables) => {
            let mut migrated = false;
            for table in tables.iter_mut() {
                migrated |= normalize_table_alias(table, legacy_key, canonical_key, relative_path)?;
            }
            Ok(migrated)
        }
        Item::Value(Value::InlineTable(table)) => {
            normalize_inline_alias(table, legacy_key, canonical_key, relative_path)
        }
        Item::Value(Value::Array(array)) => {
            let mut migrated = false;
            for value in array.iter_mut() {
                if let Some(table) = value.as_inline_table_mut() {
                    migrated |=
                        normalize_inline_alias(table, legacy_key, canonical_key, relative_path)?;
                }
            }
            Ok(migrated)
        }
        _ => Ok(false),
    }
}

/// 在普通表中移动 legacy key；重复且等值时只删除旧键，冲突时保留原文并拒绝。
///
/// @author kongweiguang
fn normalize_table_alias(
    table: &mut Table,
    legacy_key: &str,
    canonical_key: &str,
    relative_path: &Path,
) -> AppResult<bool> {
    let Some(legacy_value) = table.get(legacy_key).cloned() else {
        return Ok(false);
    };
    if let Some(canonical_value) = table.get(canonical_key) {
        if !same_scalar_value(&legacy_value, canonical_value) {
            return Err(alias_conflict(relative_path));
        }
        table.remove(legacy_key);
        return Ok(true);
    }

    let (old_key, value) = table
        .remove_entry(legacy_key)
        .ok_or_else(|| AppError::conflict("配置迁移读取别名时文档发生变化"))?;
    let new_key = Key::new(canonical_key)
        .with_leaf_decor(old_key.leaf_decor().clone())
        .with_dotted_decor(old_key.dotted_decor().clone());
    table.insert_formatted(&new_key, value);
    Ok(true)
}

/// 在内联表数组中完成同一 alias 转换，保持变量和上下文绑定结构不变。
///
/// @author kongweiguang
fn normalize_inline_alias(
    table: &mut InlineTable,
    legacy_key: &str,
    canonical_key: &str,
    relative_path: &Path,
) -> AppResult<bool> {
    let Some(legacy_value) = table.get(legacy_key).cloned() else {
        return Ok(false);
    };
    if let Some(canonical_value) = table.get(canonical_key) {
        if !same_scalar_value(
            &Item::Value(legacy_value),
            &Item::Value(canonical_value.clone()),
        ) {
            return Err(alias_conflict(relative_path));
        }
        table.remove(legacy_key);
        return Ok(true);
    }

    let (old_key, value) = table
        .remove_entry(legacy_key)
        .ok_or_else(|| AppError::conflict("配置迁移读取别名时文档发生变化"))?;
    let new_key = Key::new(canonical_key)
        .with_leaf_decor(old_key.leaf_decor().clone())
        .with_dotted_decor(old_key.dotted_decor().clone());
    table.insert_formatted(&new_key, value);
    Ok(true)
}

/// 比较别名的 TOML 标量值而忽略原格式；非标量别名不视为可安全合并。
///
/// @author kongweiguang
fn same_scalar_value(left: &Item, right: &Item) -> bool {
    let (Some(left), Some(right)) = (left.as_value(), right.as_value()) else {
        return false;
    };
    match (left, right) {
        (Value::String(left), Value::String(right)) => left.value() == right.value(),
        (Value::Integer(left), Value::Integer(right)) => left.value() == right.value(),
        (Value::Float(left), Value::Float(right)) => left.value() == right.value(),
        (Value::Boolean(left), Value::Boolean(right)) => left.value() == right.value(),
        (Value::Datetime(left), Value::Datetime(right)) => left.value() == right.value(),
        _ => false,
    }
}

/// 冲突错误不回显用户字段值或文件绝对路径，调用方可安全展示并保留备份。
///
/// @author kongweiguang
fn alias_conflict(_relative_path: &Path) -> AppError {
    AppError::conflict("配置同时包含新旧字段且值不一致，迁移已拒绝")
}

/// 从已识别的生成指南正文摘要判断是否可升级；未来和自定义内容始终无编辑计划。
///
/// @author kongweiguang
fn plan_config_guide_edit(root: &Path, edits: &mut Vec<super::FileEdit>) -> AppResult<()> {
    let relative_path = PathBuf::from("kerminal-config.md");
    let absolute_path = root.join(&relative_path);
    let metadata = match fs::symlink_metadata(&absolute_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(storage_error("读取配置指南元数据失败", error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(());
    }
    let before =
        fs::read(&absolute_path).map_err(|error| storage_error("读取配置指南失败", error))?;
    if outdated_guide_revision(&before).is_none() {
        return Ok(());
    }
    let after = configuration::managed_config_guide();
    if before != after {
        edits.push(super::FileEdit {
            relative_path,
            before: Some(before),
            after,
        });
    }
    Ok(())
}

/// Current contract roots may update only a guide whose complete body matches a frozen revision;
/// avoiding the ordinary configuration planner keeps unrelated files and values untouched.
///
/// @author kongweiguang
pub(super) fn plan_current_config_guide_edit(root: &Path) -> AppResult<Option<super::FileEdit>> {
    let mut edits = Vec::new();
    plan_config_guide_edit(root, &mut edits)?;
    Ok(edits.pop())
}

/// 只认冻结的旧正文摘要；有无生成标记都不能仅凭 revision 覆盖用户改写的指南。
///
/// @author kongweiguang
fn outdated_guide_revision(contents: &[u8]) -> Option<u32> {
    if let Some((revision, digest)) = configuration::config_guide_marker(contents) {
        if revision > kerminal_application::preferences::CONFIG_GUIDE_REVISION {
            return None;
        }
        if revision == kerminal_application::preferences::CONFIG_GUIDE_REVISION
            && digest
                == configuration::config_guide_digest(
                    kerminal_application::preferences::CONFIG_GUIDE,
                )
        {
            return None;
        }
        return (known_previous_guide_digest(revision) == Some(digest.as_str()))
            .then_some(revision);
    }

    let source = std::str::from_utf8(contents).ok()?;
    let digest = configuration::config_guide_digest(source);
    if digest == configuration::config_guide_digest(kerminal_application::preferences::CONFIG_GUIDE)
    {
        None
    } else if digest == CONFIG_GUIDE_REVISION_10_SHA256 {
        Some(10)
    } else if digest == CONFIG_GUIDE_REVISION_9_SHA256 {
        Some(9)
    } else if digest == CONFIG_GUIDE_REVISION_8_SHA256 {
        Some(8)
    } else if digest == CONFIG_GUIDE_REVISION_7_SHA256 {
        Some(7)
    } else if digest == CONFIG_GUIDE_REVISION_6_SHA256 {
        Some(6)
    } else if digest == CONFIG_GUIDE_REVISION_5_SHA256 {
        Some(5)
    } else if digest == CONFIG_GUIDE_REVISION_3_SHA256 {
        Some(3)
    } else if digest == CONFIG_GUIDE_REVISION_2_SHA256 {
        Some(2)
    } else if digest == CONFIG_GUIDE_REVISION_1_SHA256 {
        Some(1)
    } else if digest == LEGACY_TAURI_CONFIG_GUIDE_SHA256 {
        Some(0)
    } else {
        None
    }
}

/// Recovery accepts only exact frozen historical guide bodies, never arbitrary caller content.
///
/// @author kongweiguang
pub(super) fn is_frozen_config_guide_bytes(contents: &[u8]) -> bool {
    outdated_guide_revision(contents).is_some()
}

/// 为显式支持的历史 revision 返回固定摘要，未冻结的旧版和用户自行重签的正文均不升级。
///
/// @author kongweiguang
fn known_previous_guide_digest(revision: u32) -> Option<&'static str> {
    match revision {
        0 => Some(LEGACY_TAURI_CONFIG_GUIDE_SHA256),
        1 => Some(CONFIG_GUIDE_REVISION_1_SHA256),
        2 => Some(CONFIG_GUIDE_REVISION_2_SHA256),
        3 => Some(CONFIG_GUIDE_REVISION_3_SHA256),
        5 => Some(CONFIG_GUIDE_REVISION_5_SHA256),
        6 => Some(CONFIG_GUIDE_REVISION_6_SHA256),
        7 => Some(CONFIG_GUIDE_REVISION_7_SHA256),
        8 => Some(CONFIG_GUIDE_REVISION_8_SHA256),
        9 => Some(CONFIG_GUIDE_REVISION_9_SHA256),
        10 => Some(CONFIG_GUIDE_REVISION_10_SHA256),
        _ => None,
    }
}

/// 读取 schema 版本只用于区分可升级 host 与其它被当前 validator 拒绝的文档。
///
/// @author kongweiguang
fn source_schema_version(source: &[u8]) -> Option<i64> {
    let source = std::str::from_utf8(source).ok()?;
    let document = source.parse::<DocumentMut>().ok()?;
    document.get("schema_version").and_then(Item::as_integer)
}

/// 规范化路径分隔符以匹配 validator 面向用户的相对路径诊断。
///
/// @author kongweiguang
fn normalized_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// 保留底层文件系统原因供迁移入口记录，同时不给错误文案拼入文件内容。
///
/// @author kongweiguang
fn storage_error<E>(context: &'static str, source: E) -> AppError
where
    E: std::error::Error + Send + Sync + 'static,
{
    AppError::infrastructure_with_source(context, source)
}

#[cfg(test)]
#[path = "../../tests/support/data_migration_configuration_unit.rs"]
mod tests;
