//! 冻结的 v0.3.45 只读片段目录数据及其领域投影。
//!
//! 数据来自 Kerminal 自有的 `src-tauri/snippet-presets/v1`，固定基线为
//! `17c5d4be46172ca508f92546bfae5fe5ef477f11`；仅复用项目维护的 TOML 数据，
//! 不引入旧 Tauri 服务层或构建脚本。
//!
//! @author kongweiguang

use std::{collections::HashSet, sync::LazyLock};

use kerminal_application::{AppError, AppResult};
use kerminal_domain::commands::{
    SnippetCatalogItem, SnippetCatalogOrigin, SnippetContextBinding, SnippetDefaultAction,
    SnippetRisk, SnippetScope, SnippetVariable, SnippetVariableKind, SnippetVariableRenderStrategy,
};
use serde::Deserialize;

static BUILTIN_CATALOG: LazyLock<Result<Vec<SnippetCatalogItem>, ()>> =
    LazyLock::new(load_builtin_catalog);

const BUILTIN_DOCUMENTS: &[(&str, &str)] = &[
    (
        "core/auth-failures.toml",
        include_str!("../../resources/snippet-presets/v1/core/auth-failures.toml"),
    ),
    (
        "core/current-directory.toml",
        include_str!("../../resources/snippet-presets/v1/core/current-directory.toml"),
    ),
    (
        "core/dns-lookup.toml",
        include_str!("../../resources/snippet-presets/v1/core/dns-lookup.toml"),
    ),
    (
        "core/docker-containers.toml",
        include_str!("../../resources/snippet-presets/v1/core/docker-containers.toml"),
    ),
    (
        "core/docker-disk.toml",
        include_str!("../../resources/snippet-presets/v1/core/docker-disk.toml"),
    ),
    (
        "core/docker-logs.toml",
        include_str!("../../resources/snippet-presets/v1/core/docker-logs.toml"),
    ),
    (
        "core/docker-resources.toml",
        include_str!("../../resources/snippet-presets/v1/core/docker-resources.toml"),
    ),
    (
        "core/docker-stats.toml",
        include_str!("../../resources/snippet-presets/v1/core/docker-stats.toml"),
    ),
    (
        "core/failed-services.toml",
        include_str!("../../resources/snippet-presets/v1/core/failed-services.toml"),
    ),
    (
        "core/find-large-files.toml",
        include_str!("../../resources/snippet-presets/v1/core/find-large-files.toml"),
    ),
    (
        "core/git-status.toml",
        include_str!("../../resources/snippet-presets/v1/core/git-status.toml"),
    ),
    (
        "core/http-head.toml",
        include_str!("../../resources/snippet-presets/v1/core/http-head.toml"),
    ),
    (
        "core/large-files.toml",
        include_str!("../../resources/snippet-presets/v1/core/large-files.toml"),
    ),
    (
        "core/listening-ports.toml",
        include_str!("../../resources/snippet-presets/v1/core/listening-ports.toml"),
    ),
    (
        "core/network-addresses.toml",
        include_str!("../../resources/snippet-presets/v1/core/network-addresses.toml"),
    ),
    (
        "core/network-routes.toml",
        include_str!("../../resources/snippet-presets/v1/core/network-routes.toml"),
    ),
    (
        "core/network-summary.toml",
        include_str!("../../resources/snippet-presets/v1/core/network-summary.toml"),
    ),
    (
        "core/ping-host.toml",
        include_str!("../../resources/snippet-presets/v1/core/ping-host.toml"),
    ),
    (
        "core/process-cpu.toml",
        include_str!("../../resources/snippet-presets/v1/core/process-cpu.toml"),
    ),
    (
        "core/process-memory.toml",
        include_str!("../../resources/snippet-presets/v1/core/process-memory.toml"),
    ),
    (
        "core/search-text.toml",
        include_str!("../../resources/snippet-presets/v1/core/search-text.toml"),
    ),
    (
        "core/service-journal.toml",
        include_str!("../../resources/snippet-presets/v1/core/service-journal.toml"),
    ),
    (
        "core/service-status.toml",
        include_str!("../../resources/snippet-presets/v1/core/service-status.toml"),
    ),
    (
        "core/system-errors.toml",
        include_str!("../../resources/snippet-presets/v1/core/system-errors.toml"),
    ),
    (
        "core/system-overview.toml",
        include_str!("../../resources/snippet-presets/v1/core/system-overview.toml"),
    ),
    (
        "core/top-cpu-processes.toml",
        include_str!("../../resources/snippet-presets/v1/core/top-cpu-processes.toml"),
    ),
    (
        "core/top-memory-processes.toml",
        include_str!("../../resources/snippet-presets/v1/core/top-memory-processes.toml"),
    ),
    (
        "database/database-containers.toml",
        include_str!("../../resources/snippet-presets/v1/database/database-containers.toml"),
    ),
    (
        "database/database-ports.toml",
        include_str!("../../resources/snippet-presets/v1/database/database-ports.toml"),
    ),
    (
        "database/database-processes.toml",
        include_str!("../../resources/snippet-presets/v1/database/database-processes.toml"),
    ),
    (
        "development/git-pull-ff.toml",
        include_str!("../../resources/snippet-presets/v1/development/git-pull-ff.toml"),
    ),
    (
        "development/git-status.toml",
        include_str!("../../resources/snippet-presets/v1/development/git-status.toml"),
    ),
    (
        "development/npm-build.toml",
        include_str!("../../resources/snippet-presets/v1/development/npm-build.toml"),
    ),
    (
        "orchestration/compose-status.toml",
        include_str!("../../resources/snippet-presets/v1/orchestration/compose-status.toml"),
    ),
    (
        "orchestration/helm-releases.toml",
        include_str!("../../resources/snippet-presets/v1/orchestration/helm-releases.toml"),
    ),
    (
        "orchestration/kubernetes-pods.toml",
        include_str!("../../resources/snippet-presets/v1/orchestration/kubernetes-pods.toml"),
    ),
    (
        "web/http-timing.toml",
        include_str!("../../resources/snippet-presets/v1/web/http-timing.toml"),
    ),
    (
        "web/web-listeners.toml",
        include_str!("../../resources/snippet-presets/v1/web/web-listeners.toml"),
    ),
    (
        "web/web-processes.toml",
        include_str!("../../resources/snippet-presets/v1/web/web-processes.toml"),
    ),
];

/// 构造内置目录副本供 repository 过滤排序；解析只发生一次且错误不会伪装为空目录。
pub(super) fn builtin_snippet_catalog() -> AppResult<Vec<SnippetCatalogItem>> {
    BUILTIN_CATALOG
        .as_ref()
        .map(Clone::clone)
        .map_err(|_| AppError::infrastructure("内置片段目录资源无效"))
}

/// 校验固定资源身份及 pack/category 顺序唯一性，再映射为 UI 无关的领域目录 DTO。
fn load_builtin_catalog() -> Result<Vec<SnippetCatalogItem>, ()> {
    let mut items = Vec::with_capacity(BUILTIN_DOCUMENTS.len());
    let mut identities = HashSet::with_capacity(BUILTIN_DOCUMENTS.len());
    let mut positions = HashSet::with_capacity(BUILTIN_DOCUMENTS.len());
    for (_, source) in BUILTIN_DOCUMENTS {
        let document: BuiltinPresetDocument = toml::from_str(source).map_err(|_| ())?;
        if document.schema_version != 1
            || !document.id.starts_with("snippet.builtin.")
            || document.sort_order == 0
            || document.title.trim().is_empty()
            || document.description.trim().is_empty()
            || document.template.trim().is_empty()
            || document.command_spec.trim().is_empty()
            || document.owner.trim().is_empty()
            || document.tested_version.trim().is_empty()
            || document.updated_at.trim().is_empty()
            || document.source.name.trim().is_empty()
            || document.source.url.trim().is_empty()
            || document.platforms.is_empty()
            || document.shells.is_empty()
        {
            return Err(());
        }
        if !identities.insert(document.id.clone())
            || !positions.insert((
                document.pack.clone(),
                document.category.clone(),
                document.sort_order,
            ))
        {
            return Err(());
        }
        items.push(document.into_catalog_item());
    }
    Ok(items)
}

/// 固定资源 DTO 与旧 v2 build schema 对齐，未知字段会让静态目录显式不可用。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BuiltinPresetDocument {
    schema_version: u32,
    id: String,
    catalog_version: String,
    pack: String,
    category: String,
    sort_order: u32,
    title: String,
    description: String,
    template: String,
    command_spec: String,
    scope: SnippetScope,
    platforms: Vec<BuiltinPlatform>,
    shells: Vec<BuiltinShell>,
    capabilities: Vec<String>,
    tags: Vec<String>,
    risk: SnippetRisk,
    #[serde(default)]
    sensitive: bool,
    duration: BuiltinDuration,
    default_action: SnippetDefaultAction,
    #[serde(default)]
    variables: Vec<BuiltinVariableDocument>,
    source: BuiltinSourceDocument,
    owner: String,
    tested_version: String,
    updated_at: String,
    #[serde(default)]
    deprecated: bool,
}

impl BuiltinPresetDocument {
    /// 将已固定的旧目录字段投影到新领域 DTO，不把 Tauri runtime 类型带入新工程。
    fn into_catalog_item(self) -> SnippetCatalogItem {
        let updated_at = self.updated_at.clone();
        let variables = self
            .variables
            .into_iter()
            .map(BuiltinVariableDocument::into_domain)
            .collect();
        SnippetCatalogItem {
            id: self.id,
            origin: SnippetCatalogOrigin::Builtin,
            title: self.title,
            description: self.description,
            template: self.template,
            category: self.category,
            pack: self.pack,
            tags: self.tags,
            scope: self.scope,
            platforms: self
                .platforms
                .iter()
                .map(|platform| platform.value().to_owned())
                .collect(),
            shells: self
                .shells
                .iter()
                .map(|shell| shell.value().to_owned())
                .collect(),
            capabilities: self.capabilities,
            risk: self.risk,
            sensitive: self.sensitive,
            duration: self.duration.value().to_owned(),
            default_action: self.default_action,
            variables,
            context_bindings: Vec::<SnippetContextBinding>::new(),
            derived_from: None,
            catalog_version: Some(self.catalog_version),
            source_name: Some(self.source.name),
            source_url: Some(self.source.url),
            command_spec: Some(self.command_spec),
            owner: Some(self.owner),
            tested_version: Some(self.tested_version),
            deprecated: self.deprecated,
            favorite: false,
            use_count: 0,
            last_used_at_unix_ms: None,
            sort_order: i64::from(self.sort_order),
            updated_at,
        }
    }
}

/// 变量 DTO 单独转换 snake_case render strategy，避免 serde 字符串变成存储契约。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BuiltinVariableDocument {
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
    render_strategy: BuiltinRenderStrategy,
    #[serde(default)]
    sensitive: bool,
}

impl BuiltinVariableDocument {
    /// 只将旧变量的公开显示与安全策略字段映射到领域类型。
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
            render_strategy: self.render_strategy.into_domain(),
            sensitive: self.sensitive,
        }
    }
}

/// 旧目录枚举用 snake_case，投影仍使用平台稳定标识。
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum BuiltinPlatform {
    Windows,
    Macos,
    Linux,
}

impl BuiltinPlatform {
    /// 保持 v2 UI 的平台标识，不推导当前机器可执行能力。
    const fn value(&self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Macos => "macos",
            Self::Linux => "linux",
        }
    }
}

/// 旧 Windows PowerShell token 经 v2 projection 转成 `powerShell`。
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum BuiltinShell {
    Bash,
    Zsh,
    Fish,
    #[serde(rename = "power_shell")]
    PowerShell,
    Cmd,
}

impl BuiltinShell {
    /// 将固定配置 token 转为旧 MCP/UI 稳定 shell 名称。
    const fn value(&self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
            Self::PowerShell => "powerShell",
            Self::Cmd => "cmd",
        }
    }
}

/// 旧 schema 的 `high_io` 拼写由此类型保留，外部目录直接使用稳定字符串。
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum BuiltinDuration {
    Instant,
    Streaming,
    #[serde(rename = "high_io")]
    HighIo,
}

impl BuiltinDuration {
    /// 复用 v2 目录的 duration 枚举值，而不把其运行规则移入领域层。
    const fn value(&self) -> &'static str {
        match self {
            Self::Instant => "instant",
            Self::Streaming => "streaming",
            Self::HighIo => "high_io",
        }
    }
}

/// 旧 source 子表仅承载上游资料名称和链接，不携带执行或下载能力。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BuiltinSourceDocument {
    name: String,
    url: String,
}

/// 旧 render strategy 固定使用 snake_case，再转换成领域 enum。
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum BuiltinRenderStrategy {
    ShellArg,
    ValidatedRaw,
    Literal,
}

impl BuiltinRenderStrategy {
    /// 显式映射策略，未来改名不能悄悄改变内置变量的转义方式。
    const fn into_domain(self) -> SnippetVariableRenderStrategy {
        match self {
            Self::ShellArg => SnippetVariableRenderStrategy::ShellArg,
            Self::ValidatedRaw => SnippetVariableRenderStrategy::ValidatedRaw,
            Self::Literal => SnippetVariableRenderStrategy::Literal,
        }
    }
}
