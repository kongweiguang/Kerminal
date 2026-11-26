//! 已解压目录的统一清点与引用验证；归档读取方仍须在解压前拒绝危险条目。
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use crate::{
    ConfigurationSchema, EmptyState, IconReference, ManifestPolicy, MenuItem, NativeNode,
    NativeNodeKind, NativeView, Platform, PluginManifest, PluginRuntime, ProtocolError,
    ViewDefinition, classify_icon_reference, invalid, parse_json, parse_manifest,
    parse_native_view,
};

pub const MAX_PACKAGE_ENTRIES: usize = 10_000;
pub const MAX_PACKAGE_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_PACKAGE_FILE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageFile {
    pub path: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedPackage {
    pub manifest: PluginManifest,
    pub files: BTreeMap<String, PackageFile>,
    pub views: BTreeMap<String, NativeView>,
    pub configuration_schemas: BTreeMap<String, ConfigurationSchema>,
}

impl ValidatedPackage {
    /// 平台包只需包含当前平台的 Sidecar，但必须由安装器核对目标平台。
    pub fn validate_for_platform(&self, platform: Platform) -> Result<(), ProtocolError> {
        match &self.manifest.runtime {
            PluginRuntime::None { .. } if platform == Platform::Any => Ok(()),
            PluginRuntime::Sidecar { main, .. } if platform != Platform::Any => {
                let path = main
                    .get(&platform)
                    .ok_or_else(|| invalid("runtime.main", "未声明目标平台"))?;
                require_file(&self.files, path)
            }
            _ => Err(invalid("package.platform", "包平台与插件运行类型不匹配")),
        }
    }
}

/// 对可移植相对路径采用统一规则，包括在非 Windows 环境拒绝 Windows 别名。
pub fn validate_package_path(path: &str) -> Result<(), ProtocolError> {
    if path.is_empty() || path.starts_with('/') || path.contains(['\\', ':', '\0']) {
        return Err(invalid("package.path", "路径必须是正斜杠分隔的相对路径"));
    }
    for segment in path.split('/') {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.ends_with(['.', ' '])
            || segment.chars().any(|character| {
                character.is_control() || matches!(character, '<' | '>' | '"' | '|' | '?' | '*')
            })
        {
            return Err(invalid("package.path", "路径段无效"));
        }
        let stem = segment.split('.').next().unwrap_or_default().to_uppercase();
        let reserved = matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|number| {
                matches!(
                    number,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
        if reserved {
            return Err(invalid("package.path", "路径使用 Windows 保留名"));
        }
    }
    Ok(())
}

/// 唯一的目录校验入口同时清点文件、检查所有引用并解析静态视图和配置 schema。
pub fn validate_package(
    root: &Path,
    policy: ManifestPolicy,
) -> Result<ValidatedPackage, ProtocolError> {
    let files = inventory(root)?;
    let manifest_bytes = read_package_file(
        root,
        &files,
        crate::MANIFEST_FILE_NAME,
        crate::MAX_MANIFEST_BYTES,
    )?;
    let manifest = parse_manifest(&manifest_bytes, policy)?;
    if let Some(icon) = &manifest.icon
        && classify_icon_reference(icon)? == IconReference::PackagePath
    {
        require_file(&files, icon)?;
    }
    match &manifest.runtime {
        PluginRuntime::None { .. } => {}
        PluginRuntime::Sidecar { main, .. } => {
            if !main.values().any(|path| files.contains_key(path)) {
                return Err(invalid("runtime.main", "包内没有任何已声明的 Sidecar"));
            }
        }
    }
    for tool in &manifest.contributes.tools {
        if let Some(icon) = &tool.icon
            && classify_icon_reference(icon)? == IconReference::PackagePath
        {
            require_file(&files, icon)?;
        }
    }
    let mut views = BTreeMap::new();
    let mut schema_paths = BTreeSet::new();
    let mut l10n_keys = BTreeSet::new();
    for tool in &manifest.contributes.tools {
        collect_l10n(&tool.title, &mut l10n_keys);
    }
    for tab in &manifest.contributes.tabs {
        collect_l10n(&tab.title, &mut l10n_keys);
    }
    for host in &manifest.contributes.host_types {
        collect_l10n(&host.title, &mut l10n_keys);
        schema_paths.insert(host.schema.clone());
    }
    for command in &manifest.contributes.commands {
        collect_l10n(&command.title, &mut l10n_keys);
    }
    if let Some(settings) = &manifest.contributes.settings {
        schema_paths.insert(settings.schema.clone());
    }
    for (id, view) in &manifest.views {
        match view {
            ViewDefinition::Native { file, .. } => {
                let bytes = read_package_file(root, &files, file, crate::MAX_VIEW_BYTES)?;
                let parsed = parse_native_view(&bytes)?;
                inspect_node_references(&parsed.root, &files, &mut schema_paths, &mut l10n_keys)?;
                views.insert(id.clone(), parsed);
            }
            ViewDefinition::Webview { entry, .. } => {
                require_file(&files, entry)?;
            }
        }
    }
    let mut configuration_schemas = BTreeMap::new();
    for path in schema_paths {
        let bytes = read_package_file(root, &files, &path, crate::MAX_CONFIGURATION_SCHEMA_BYTES)?;
        configuration_schemas.insert(path, ConfigurationSchema::parse(&bytes)?);
    }
    validate_localizations(root, &files, manifest.l10n.as_deref(), &l10n_keys)?;
    Ok(ValidatedPackage {
        manifest,
        files,
        views,
        configuration_schemas,
    })
}

/// 只扫描实际解压目录；归档阶段必须单独限制原始条目类型和重复路径。
fn inventory(root: &Path) -> Result<BTreeMap<String, PackageFile>, ProtocolError> {
    let root_metadata = fs::symlink_metadata(root).map_err(|source| ProtocolError::Io {
        location: "package.root".into(),
        source,
    })?;
    if !root_metadata.is_dir() || is_link(&root_metadata) {
        return Err(invalid("package.root", "包根目录必须是普通目录"));
    }
    let mut pending = vec![(root.to_path_buf(), String::new())];
    let mut names = BTreeSet::new();
    let mut upper_names = BTreeSet::new();
    let mut files = BTreeMap::new();
    let mut count = 0usize;
    let mut total = 0u64;
    while let Some((directory, relative)) = pending.pop() {
        let entries = fs::read_dir(&directory).map_err(|source| ProtocolError::Io {
            location: "package.directory".into(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| ProtocolError::Io {
                location: "package.entry".into(),
                source,
            })?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| invalid("package.path", "路径不是 UTF-8"))?;
            let path = if relative.is_empty() {
                name.to_owned()
            } else {
                format!("{relative}/{name}")
            };
            validate_package_path(&path)?;
            count += 1;
            if count > MAX_PACKAGE_ENTRIES
                || !names.insert(path.to_lowercase())
                || !upper_names.insert(path.to_uppercase())
            {
                return Err(invalid("package", "条目数超限或路径大小写冲突"));
            }
            let metadata =
                fs::symlink_metadata(entry.path()).map_err(|source| ProtocolError::Io {
                    location: path.clone(),
                    source,
                })?;
            if is_link(&metadata) {
                return Err(invalid("package", "不允许链接或重解析点"));
            }
            if metadata.is_dir() {
                pending.push((entry.path(), path));
            } else if metadata.is_file() {
                ensure_single_link(&entry.path(), &metadata)?;
                let size = metadata.len();
                total = total
                    .checked_add(size)
                    .ok_or_else(|| invalid("package", "解压大小溢出"))?;
                if size > MAX_PACKAGE_FILE_BYTES || total > MAX_PACKAGE_BYTES {
                    return Err(invalid("package", "解压大小超限"));
                }
                files.insert(path.clone(), PackageFile { path, size });
            } else {
                return Err(invalid("package", "只允许普通文件和目录"));
            }
        }
    }
    Ok(files)
}

/// 所有平台都拒绝符号链接；Windows 还拒绝其他重解析点。
fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink()
            || metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// 硬链接会让外部路径绕开包目录边界，因此遇到多个链接即拒绝。
#[cfg(unix)]
fn ensure_single_link(_path: &Path, metadata: &fs::Metadata) -> Result<(), ProtocolError> {
    use std::os::unix::fs::MetadataExt;
    if metadata.nlink() != 1 {
        return Err(invalid("package", "不允许硬链接"));
    }
    Ok(())
}

/// Windows 的稳定标准库不公开链接数，使用已打开文件的句柄查询。
#[cfg(windows)]
fn ensure_single_link(path: &Path, _metadata: &fs::Metadata) -> Result<(), ProtocolError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let file = fs::File::open(path).map_err(|source| ProtocolError::Io {
        location: "package.file".into(),
        source,
    })?;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let result = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) };
    if result == 0 {
        return Err(ProtocolError::Io {
            location: "package.file".into(),
            source: std::io::Error::last_os_error(),
        });
    }
    if information.nNumberOfLinks != 1 {
        return Err(invalid("package", "不允许硬链接"));
    }
    Ok(())
}

/// 无法核验链接数的平台不得把未知状态当作安全通过。
#[cfg(not(any(unix, windows)))]
fn ensure_single_link(_path: &Path, _metadata: &fs::Metadata) -> Result<(), ProtocolError> {
    Err(invalid("package", "当前平台不支持硬链接检查"))
}

/// 只接受清点中大小写完全一致的普通文件。
fn require_file(files: &BTreeMap<String, PackageFile>, path: &str) -> Result<(), ProtocolError> {
    validate_package_path(path)?;
    if !files.contains_key(path) {
        return Err(invalid(path, "引用文件不存在"));
    }
    Ok(())
}

/// 读取前检查清点结果和实际大小，避免对大型资产无界分配。
fn read_package_file(
    root: &Path,
    files: &BTreeMap<String, PackageFile>,
    path: &str,
    limit: usize,
) -> Result<Vec<u8>, ProtocolError> {
    require_file(files, path)?;
    if files[path].size > limit as u64 {
        return Err(invalid(path, "文件超过对应格式大小上限"));
    }
    let full_path: PathBuf = root.join(path);
    let file = fs::File::open(full_path).map_err(|source| ProtocolError::Io {
        location: path.into(),
        source,
    })?;
    let mut bytes = Vec::with_capacity(files[path].size as usize);
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| ProtocolError::Io {
            location: path.into(),
            source,
        })?;
    if bytes.len() > limit || bytes.len() as u64 != files[path].size {
        return Err(invalid(path, "读取期间文件大小变化"));
    }
    Ok(bytes)
}

/// 仅收集完整的翻译标记，不把普通字符串拆成表达式执行。
fn collect_l10n(text: &str, keys: &mut BTreeSet<String>) {
    if let Some(key) = text
        .strip_prefix('%')
        .and_then(|value| value.strip_suffix('%'))
        && !key.is_empty()
    {
        keys.insert(key.to_owned());
    }
}

/// 按节点类型检查真实引用，扩展数据与选项值是插件私有数据而非界面声明。
fn inspect_node_references(
    node: &NativeNode,
    files: &BTreeMap<String, PackageFile>,
    schemas: &mut BTreeSet<String>,
    keys: &mut BTreeSet<String>,
) -> Result<(), ProtocolError> {
    use NativeNodeKind::*;
    match &node.kind {
        Stack { children, .. }
        | Row { children, .. }
        | Split { children, .. }
        | Toolbar { children, .. } => {
            for child in children {
                inspect_node_references(child, files, schemas, keys)?;
            }
        }
        Section {
            title, children, ..
        } => {
            collect_l10n(title, keys);
            for child in children {
                inspect_node_references(child, files, schemas, keys)?;
            }
        }
        Scroll { child, .. } => inspect_node_references(child, files, schemas, keys)?,
        Tabs { tabs, .. } => {
            for tab in tabs {
                collect_l10n(&tab.title, keys);
                inspect_node_references(&tab.content, files, schemas, keys)?;
            }
        }
        Text { text, .. } | Markdown { text, .. } | Badge { text, .. } | Notice { text, .. } => {
            collect_l10n(text, keys)
        }
        Icon { icon, .. } => inspect_icon_reference(Some(icon), files)?,
        Empty {
            title,
            icon,
            description,
            ..
        } => {
            collect_l10n(title, keys);
            collect_optional_l10n(description.as_deref(), keys);
            inspect_icon_reference(icon.as_deref(), files)?;
        }
        Input {
            label, placeholder, ..
        }
        | Textarea {
            label, placeholder, ..
        } => {
            collect_optional_l10n(label.as_deref(), keys);
            collect_optional_l10n(placeholder.as_deref(), keys);
        }
        Number { label, .. }
        | Switch { label, .. }
        | Checkbox { label, .. }
        | Path { label, .. }
        | Secret { label, .. } => collect_optional_l10n(label.as_deref(), keys),
        Select { label, options, .. } => {
            collect_optional_l10n(label.as_deref(), keys);
            for option in options {
                collect_l10n(&option.label, keys);
            }
        }
        Button { label, icon, .. } => {
            collect_optional_l10n(label.as_deref(), keys);
            inspect_icon_reference(icon.as_deref(), files)?;
        }
        Menu { items, .. } => collect_menu_l10n(items, keys),
        List { menu, empty, .. } | Tree { menu, empty, .. } => {
            inspect_collection_references(menu.as_deref(), empty.as_ref(), files, keys)?;
        }
        Table {
            columns,
            menu,
            empty,
            ..
        } => {
            for column in columns {
                collect_l10n(&column.title, keys);
            }
            inspect_collection_references(menu.as_deref(), empty.as_ref(), files, keys)?;
        }
        Form { schema, .. } => {
            schemas.insert(schema.clone());
        }
        Qrcode { label, .. } => collect_optional_l10n(label.as_deref(), keys),
        Code { .. } | Progress { .. } => {}
    }
    Ok(())
}

/// 内置图标名由宿主解析，包含路径的图标必须在已校验包中存在。
fn inspect_icon_reference(
    icon: Option<&str>,
    files: &BTreeMap<String, PackageFile>,
) -> Result<(), ProtocolError> {
    if let Some(icon) = icon {
        match classify_icon_reference(icon)? {
            IconReference::PackagePath => require_file(files, icon)?,
            // 内置名不需要包内文件；宿主会在渲染前按内置目录安全解析，
            // 未知名称由界面回退占位图标。
            IconReference::Builtin => {}
        }
    }
    Ok(())
}

/// 可选文案缺失时不产生翻译依赖，避免把其他字段当作展示文本。
fn collect_optional_l10n(text: Option<&str>, keys: &mut BTreeSet<String>) {
    if let Some(text) = text {
        collect_l10n(text, keys);
    }
}

/// 菜单只解析标签，不解析动作标识或插件扩展。
fn collect_menu_l10n(items: &[MenuItem], keys: &mut BTreeSet<String>) {
    for item in items {
        collect_l10n(&item.label, keys);
    }
}

/// 集合的菜单和空态使用相同的引用规则，避免不同集合出现行为差异。
fn inspect_collection_references(
    menu: Option<&[MenuItem]>,
    empty: Option<&EmptyState>,
    files: &BTreeMap<String, PackageFile>,
    keys: &mut BTreeSet<String>,
) -> Result<(), ProtocolError> {
    if let Some(menu) = menu {
        collect_menu_l10n(menu, keys);
    }
    if let Some(empty) = empty {
        collect_l10n(&empty.title, keys);
        inspect_icon_reference(empty.icon.as_deref(), files)?;
    }
    Ok(())
}

/// 英文翻译必须覆盖视图使用的键并提供非空文案，避免安装通过后注册失败；其他语言允许回退英文。
fn validate_localizations(
    root: &Path,
    files: &BTreeMap<String, PackageFile>,
    directory: Option<&str>,
    keys: &BTreeSet<String>,
) -> Result<(), ProtocolError> {
    let Some(directory) = directory else {
        return if keys.is_empty() {
            Ok(())
        } else {
            Err(invalid("l10n", "使用了翻译键但未声明翻译目录"))
        };
    };
    let english_path = format!("{directory}/en.json");
    let prefix = format!("{directory}/");
    let mut english = None;
    for path in files
        .keys()
        .filter(|path| path.starts_with(&prefix) && path.ends_with(".json"))
    {
        let bytes = read_package_file(root, files, path, 1024 * 1024)?;
        let value = parse_json(&bytes, "l10n")?;
        let object = value
            .as_object()
            .ok_or_else(|| invalid("l10n", "翻译文件必须是对象"))?;
        if object.values().any(|value| !value.is_string()) {
            return Err(invalid("l10n", "翻译值必须是字符串"));
        }
        if *path == english_path {
            english = Some(object.clone());
        }
    }
    let english = english.ok_or_else(|| invalid("l10n", "缺少 en.json"))?;
    if keys.iter().any(|key| {
        english
            .get(key)
            .and_then(|value| value.as_str())
            .is_none_or(|text| text.trim().is_empty())
    }) {
        return Err(invalid("l10n", "英文翻译缺少引用的键或文案为空"));
    }
    Ok(())
}
