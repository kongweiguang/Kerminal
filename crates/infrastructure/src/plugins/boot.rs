//! 启动时冻结插件选择，并把损坏隔离在单个插件范围内。
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use kerminal_application::external_contributions::PreparedPlugin;
use kerminal_domain::plugins::{
    InstalledPlugin, PendingPluginChange, PluginBootMarker, PluginBootPhase, PluginId,
    PluginSource, PluginStoreState, PluginVersion,
};
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};

use super::store::{PluginStore, PluginStoreError, PluginStoreSnapshot};

/// 宿主通过同一协议校验器实现该端口，启动和安装不能各自解释清单。
pub trait BootPackageValidator {
    type Manifest: Clone;

    /// 调用方已限定版本目录；实现仍须核对清单、包内容、身份与 API 兼容性。
    fn validate(
        &self,
        directory: &Path,
        id: &PluginId,
        version: &PluginVersion,
        source: PluginSource,
    ) -> Result<Self::Manifest, BootPackageError>;
}

/// 启动与安装共用 protocol crate 的包校验，不在基础设施复制清单规则。
pub struct ProtocolBootValidator {
    pub host_api: String,
    pub platform: kerminal_plugin_protocol::Platform,
}

impl BootPackageValidator for ProtocolBootValidator {
    type Manifest = PreparedPlugin;

    /// 来源授权保留命名空间；身份、API 与当前平台都必须与已安装记录一致。
    fn validate(
        &self,
        directory: &Path,
        id: &PluginId,
        version: &PluginVersion,
        source: PluginSource,
    ) -> Result<Self::Manifest, BootPackageError> {
        use kerminal_plugin_protocol::{ManifestPolicy, Platform, PluginRuntime, ProtocolError};

        let policy = if source == PluginSource::Official && id.as_str().starts_with("kerminal.") {
            ManifestPolicy::Official
        } else {
            ManifestPolicy::ThirdParty
        };
        let package =
            kerminal_plugin_protocol::validate_package(directory, policy).map_err(|error| {
                if matches!(error, ProtocolError::Io { .. }) {
                    BootPackageError::Unavailable
                } else {
                    BootPackageError::Corrupt
                }
            })?;
        if package.manifest.id != id.as_str() || package.manifest.version != version.to_string() {
            return Err(BootPackageError::Corrupt);
        }
        if !package
            .manifest
            .supports_host_api(&self.host_api)
            .map_err(|_| BootPackageError::Corrupt)?
        {
            return Err(BootPackageError::Incompatible);
        }
        let platform = match &package.manifest.runtime {
            PluginRuntime::None { .. } | PluginRuntime::Script { .. } => Platform::Any,
            PluginRuntime::Sidecar { .. } => self.platform,
        };
        package
            .validate_for_platform(platform)
            .map_err(|_| BootPackageError::Incompatible)?;
        prepare_plugin(directory, id, version, package)
    }
}

const MAX_LOCALE_BYTES: u64 = 1024 * 1024;
const MAX_UI_RESOURCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_UI_SNAPSHOT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_UI_SNAPSHOT_FILES: usize = 1024;

/// 从同一受验包形成不可变快照；本函数只应在启动工作线程或后台执行器调用。
fn prepare_plugin(
    directory: &Path,
    id: &PluginId,
    version: &PluginVersion,
    package: kerminal_plugin_protocol::ValidatedPackage,
) -> Result<PreparedPlugin, BootPackageError> {
    use kerminal_plugin_protocol::ViewDefinition;

    let mut localizations = BTreeMap::new();
    let mut total = 0u64;
    if let Some(l10n) = &package.manifest.l10n {
        let prefix = format!("{l10n}/");
        for (path, metadata) in &package.files {
            let Some(locale) = path
                .strip_prefix(&prefix)
                .and_then(|name| name.strip_suffix(".json"))
            else {
                continue;
            };
            let bytes = read_snapshot_file(directory, path, metadata.size, MAX_LOCALE_BYTES)?;
            total = total
                .checked_add(bytes.len() as u64)
                .ok_or(BootPackageError::Corrupt)?;
            if total > MAX_UI_SNAPSHOT_BYTES || localizations.len() >= MAX_UI_SNAPSHOT_FILES {
                return Err(BootPackageError::Corrupt);
            }
            let dictionary: LocaleDictionary =
                serde_json::from_slice(&bytes).map_err(|_| BootPackageError::Corrupt)?;
            localizations.insert(locale.to_owned(), dictionary.0);
        }
        if !localizations.contains_key("en") {
            return Err(BootPackageError::Corrupt);
        }
    }

    let mut native_views = BTreeMap::new();
    let mut resource_paths = BTreeSet::new();
    if let Some(icon) = &package.manifest.icon {
        resource_paths.insert(icon.clone());
    }
    for tool in &package.manifest.contributes.tools {
        if let Some(icon) = &tool.icon {
            resource_paths.insert(icon.clone());
        }
    }
    for (view_id, view) in &package.views {
        let value = serde_json::to_value(view).map_err(|_| BootPackageError::Corrupt)?;
        collect_icon_paths(&value, &package.files, &mut resource_paths);
        native_views.insert(view_id.clone(), Arc::new(value));
    }
    if package
        .manifest
        .views
        .values()
        .any(|view| matches!(view, ViewDefinition::Webview { .. }))
    {
        resource_paths.extend(
            package
                .files
                .keys()
                .filter(|path| is_webview_resource(path))
                .cloned(),
        );
    }
    let mut resources = BTreeMap::new();
    // 翻译字典和网页资源分别计费，避免同一 JSON 作为网页资源时被重复扣减限额。
    let mut total = 0u64;
    for path in resource_paths {
        let file = package.files.get(&path).ok_or(BootPackageError::Corrupt)?;
        let bytes = read_snapshot_file(directory, &path, file.size, MAX_UI_RESOURCE_BYTES)?;
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or(BootPackageError::Corrupt)?;
        if total > MAX_UI_SNAPSHOT_BYTES || resources.len() >= MAX_UI_SNAPSHOT_FILES {
            return Err(BootPackageError::Corrupt);
        }
        resources.insert(path, Arc::<[u8]>::from(bytes));
    }
    Ok(PreparedPlugin {
        id: id.clone(),
        version: version.clone(),
        directory: directory.to_path_buf(),
        package: Arc::new(package),
        native_views,
        localizations,
        resources,
    })
}

/// 图标值可能深藏在容器节点中；只收集清点内的实际文件，不把图标名当作路径。
fn collect_icon_paths(
    node: &serde_json::Value,
    files: &BTreeMap<String, kerminal_plugin_protocol::PackageFile>,
    paths: &mut BTreeSet<String>,
) {
    match node {
        serde_json::Value::Object(map) => {
            if let Some(icon) = map.get("icon").and_then(serde_json::Value::as_str)
                && files.contains_key(icon)
            {
                paths.insert(icon.to_owned());
            }
            for child in map.values() {
                collect_icon_paths(child, files, paths);
            }
        }
        serde_json::Value::Array(array) => {
            for child in array {
                collect_icon_paths(child, files, paths);
            }
        }
        _ => {}
    }
}

/// 资源类型与 WebView 宿主接受的白名单保持一致，不能从扩展名回退到任意文件。
fn is_webview_resource(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(
            extension.to_ascii_lowercase().as_str(),
            "html"
                | "htm"
                | "js"
                | "mjs"
                | "css"
                | "json"
                | "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "ico"
                | "svg"
                | "woff"
                | "woff2"
                | "ttf"
                | "otf"
        )
    })
}

/// 再次检查路径每一级及打开句柄，防止资源阶段跟随链接或读取超过清点大小。
fn read_snapshot_file(
    root: &Path,
    path: &str,
    expected: u64,
    limit: u64,
) -> Result<Vec<u8>, BootPackageError> {
    kerminal_plugin_protocol::validate_package_path(path).map_err(|_| BootPackageError::Corrupt)?;
    if expected > limit {
        return Err(BootPackageError::Corrupt);
    }
    let mut current = root.to_path_buf();
    let mut segments = path.split('/').peekable();
    while let Some(segment) = segments.next() {
        current.push(segment);
        let metadata = fs::symlink_metadata(&current).map_err(|_| BootPackageError::Unavailable)?;
        if !safe_metadata(&metadata)
            || if segments.peek().is_some() {
                !metadata.is_dir()
            } else {
                !metadata.is_file()
            }
        {
            return Err(BootPackageError::Corrupt);
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options
        .open(current)
        .map_err(|_| BootPackageError::Unavailable)?;
    let metadata = file.metadata().map_err(|_| BootPackageError::Unavailable)?;
    if !metadata.is_file() || !safe_metadata(&metadata) || metadata.len() != expected {
        return Err(BootPackageError::Corrupt);
    }
    ensure_single_open_link(&file, &metadata)?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| BootPackageError::Unavailable)?;
    if bytes.len() as u64 != expected {
        return Err(BootPackageError::Corrupt);
    }
    Ok(bytes)
}

/// Unix 从打开的文件句柄核对链接数，避免资源读取绕过包目录边界。
#[cfg(unix)]
fn ensure_single_open_link(_file: &File, metadata: &fs::Metadata) -> Result<(), BootPackageError> {
    use std::os::unix::fs::MetadataExt;
    if metadata.nlink() != 1 {
        return Err(BootPackageError::Corrupt);
    }
    Ok(())
}

/// Windows 标准库不暴露链接数，复用协议校验器相同的句柄信息检查。
#[cfg(windows)]
fn ensure_single_open_link(file: &File, _metadata: &fs::Metadata) -> Result<(), BootPackageError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(BootPackageError::Unavailable);
    }
    if information.nNumberOfLinks != 1 {
        return Err(BootPackageError::Corrupt);
    }
    Ok(())
}

/// 无法确认硬链接的平台必须拒绝读取，而不是把未知风险当作已验证。
#[cfg(not(any(unix, windows)))]
fn ensure_single_open_link(_file: &File, _metadata: &fs::Metadata) -> Result<(), BootPackageError> {
    Err(BootPackageError::Unavailable)
}

/// 读取翻译时保留协议的重复键拒绝语义，避免与已验证的解释产生分歧。
struct LocaleDictionary(BTreeMap<String, String>);

impl<'de> Deserialize<'de> for LocaleDictionary {
    /// 只接受字符串字典；重复键不允许被 JSON 解析器静默覆盖。
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DictionaryVisitor;
        impl<'de> Visitor<'de> for DictionaryVisitor {
            type Value = LocaleDictionary;

            /// 错误只描述格式，避免回显插件提供的翻译内容。
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a translation object without duplicate keys")
            }

            /// 逐项检查重复身份，不接受嵌套结构或非字符串翻译。
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, String>()? {
                    if entries.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate translation key"));
                    }
                }
                Ok(LocaleDictionary(entries))
            }
        }
        deserializer.deserialize_map(DictionaryVisitor)
    }
}

/// 固定错误类别供界面展示，避免把不可信清单正文和本机路径带入状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootPackageError {
    Missing,
    Corrupt,
    Incompatible,
    Unavailable,
}

/// 安装选择与声明一并冻结；禁用插件仍可提供静态主机配置校验信息。
#[derive(Clone, Debug)]
pub struct BootPlugin<M> {
    pub id: PluginId,
    pub version: PluginVersion,
    pub directory: PathBuf,
    pub enabled: bool,
    pub manifest: M,
}

/// 快照只在 CAS 提交后返回，调用方不能注册未提交的待生效版本。
#[derive(Clone, Debug)]
pub struct BootSnapshot<M> {
    pub boot_id: String,
    pub safe_mode: bool,
    pub store: PluginStoreSnapshot,
    pub plugins: Vec<BootPlugin<M>>,
    pub rejected: BTreeMap<PluginId, BootPackageError>,
    pub cleanup_failed: bool,
}

/// 离线快照同时返回可用声明和拒绝原因，不伪造启动事务标记。
pub type StaticPluginSnapshot<M> = (Vec<BootPlugin<M>>, BTreeMap<PluginId, BootPackageError>);

impl<M> BootSnapshot<M> {
    /// 安全模式与禁用状态只影响加载，不抹除已校验的声明和持久身份。
    pub fn enabled_plugins(&self) -> impl Iterator<Item = &BootPlugin<M>> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.enabled && !self.safe_mode)
    }
}

impl PluginStore {
    /// 离线工具只读取已生效版本，不创建锁、应用待处理操作或写启动标记。
    /// 两次读取状态确保包校验期间安装选择没有改变；不可用包保留为拒绝项。
    pub fn read_static_plugins<V: BootPackageValidator>(
        &self,
        validator: &V,
    ) -> Result<StaticPluginSnapshot<V::Manifest>, PluginStoreError> {
        let before = self.read_unlocked()?;
        let mut plugins = Vec::new();
        let mut rejected = BTreeMap::new();
        for (id, installed) in &before.state.plugins {
            match read_package(self, validator, id, &installed.active, installed.source) {
                Ok((directory, manifest)) => plugins.push(BootPlugin {
                    id: id.clone(),
                    version: installed.active.clone(),
                    directory,
                    enabled: installed.enabled,
                    manifest,
                }),
                Err(reason) => {
                    rejected.insert(id.clone(), reason);
                }
            }
        }
        if self.read_unlocked()?.state != before.state {
            return Err(PluginStoreError::Conflict);
        }
        Ok((plugins, rejected))
    }

    /// 按持久顺序应用操作并一次提交；单个损坏插件不会阻止其他插件启动。
    pub fn begin_startup<V: BootPackageValidator>(
        &self,
        validator: &V,
        requested_safe_mode: bool,
    ) -> Result<BootSnapshot<V::Manifest>, PluginStoreError> {
        let current = self.load()?;
        let mut next = current.state.clone();
        let prior_crashes = next.boot.as_ref().map_or(0, |boot| match boot.phase {
            PluginBootPhase::RegistryFrozen => boot.consecutive_early_crashes.saturating_add(1),
            PluginBootPhase::Starting | PluginBootPhase::FirstFrame => 0,
        });
        let safe_mode = requested_safe_mode || prior_crashes >= 2;
        let boot_id = uuid::Uuid::new_v4().to_string();
        next.boot = Some(PluginBootMarker {
            id: boot_id.clone(),
            phase: PluginBootPhase::Starting,
            consecutive_early_crashes: prior_crashes,
        });

        let mut rejected = BTreeMap::new();
        let mut blocked = BTreeSet::new();
        let mut retained = Vec::new();
        for change in std::mem::take(&mut next.pending) {
            let id = change.plugin_id().clone();
            if blocked.contains(&id) {
                retained.push(change);
                continue;
            }
            match apply_change(self, validator, &mut next.plugins, &change) {
                Ok(()) => {
                    next.failures.remove(&id);
                }
                Err(reason) => {
                    blocked.insert(id.clone());
                    next.failures
                        .insert(id.clone(), failure_text(reason).to_owned());
                    rejected.insert(id, reason);
                    retained.push(change);
                }
            }
        }
        next.pending = retained;
        let mut plugins = Vec::new();
        for (id, installed) in &next.plugins {
            match read_package(self, validator, id, &installed.active, installed.source) {
                Ok((directory, manifest)) => {
                    if !blocked.contains(id) {
                        next.failures.remove(id);
                    }
                    plugins.push(BootPlugin {
                        id: id.clone(),
                        version: installed.active.clone(),
                        directory,
                        enabled: installed.enabled,
                        manifest,
                    });
                }
                Err(reason) => {
                    next.failures
                        .insert(id.clone(), failure_text(reason).to_owned());
                    rejected.insert(id.clone(), reason);
                }
            }
        }
        let store = self.compare_and_swap(&current, next)?;
        let cleanup_failed = self
            .with_locked_state(|state| {
                let staging = clean_staging(self.root());
                let packages = clean_unreferenced_packages(self.root(), state);
                staging.is_err() || packages.is_err()
            })
            .unwrap_or(true);
        Ok(BootSnapshot {
            boot_id,
            safe_mode,
            store,
            plugins,
            rejected,
            cleanup_failed,
        })
    }

    /// 只有当前启动身份可以推进标记；旧窗口的首帧回执不能清除新的崩溃证据。
    pub fn mark_boot_phase(
        &self,
        boot_id: &str,
        next_phase: PluginBootPhase,
    ) -> Result<PluginStoreSnapshot, PluginStoreError> {
        let current = self.load()?;
        let mut next = current.state.clone();
        let marker = next.boot.as_mut().ok_or(PluginStoreError::Conflict)?;
        if marker.id != boot_id
            || !matches!(
                (marker.phase, next_phase),
                (PluginBootPhase::Starting, PluginBootPhase::RegistryFrozen)
                    | (PluginBootPhase::RegistryFrozen, PluginBootPhase::FirstFrame)
            )
        {
            return Err(PluginStoreError::Conflict);
        }
        marker.phase = next_phase;
        if next_phase == PluginBootPhase::FirstFrame {
            marker.consecutive_early_crashes = 0;
        }
        self.compare_and_swap(&current, next)
    }
}

/// 同一插件较早的操作失败时保留后续操作，避免顺序被错误地重排。
fn apply_change<V: BootPackageValidator>(
    store: &PluginStore,
    validator: &V,
    plugins: &mut BTreeMap<PluginId, InstalledPlugin>,
    change: &PendingPluginChange,
) -> Result<(), BootPackageError> {
    match change {
        PendingPluginChange::Install {
            id,
            to,
            source,
            installed_at,
            ..
        } => {
            if plugins.contains_key(id) {
                return Err(BootPackageError::Corrupt);
            }
            read_package(store, validator, id, to, *source)?;
            plugins.insert(
                id.clone(),
                InstalledPlugin {
                    active: to.clone(),
                    previous: None,
                    enabled: true,
                    source: *source,
                    installed_at: installed_at.clone(),
                },
            );
        }
        PendingPluginChange::Update { id, to, .. } => {
            let installed = plugins.get_mut(id).ok_or(BootPackageError::Missing)?;
            read_package(store, validator, id, to, installed.source)?;
            if &installed.active != to {
                installed.previous = Some(std::mem::replace(&mut installed.active, to.clone()));
            }
        }
        PendingPluginChange::Rollback { id, to } => {
            let installed = plugins.get_mut(id).ok_or(BootPackageError::Missing)?;
            read_package(store, validator, id, to, installed.source)?;
            if installed.previous.as_ref() != Some(to) {
                return Err(BootPackageError::Corrupt);
            }
            installed.previous = Some(std::mem::replace(&mut installed.active, to.clone()));
        }
        PendingPluginChange::Enable { id } | PendingPluginChange::Disable { id } => {
            let installed = plugins.get_mut(id).ok_or(BootPackageError::Missing)?;
            installed.enabled = matches!(change, PendingPluginChange::Enable { .. });
        }
        PendingPluginChange::Uninstall { id } => {
            if plugins.remove(id).is_none() {
                return Err(BootPackageError::Missing);
            }
        }
    }
    Ok(())
}

/// 每级目录都必须是真实目录；验证器只收到由宿主推导的版本路径。
fn read_package<V: BootPackageValidator>(
    store: &PluginStore,
    validator: &V,
    id: &PluginId,
    version: &PluginVersion,
    source: PluginSource,
) -> Result<(PathBuf, V::Manifest), BootPackageError> {
    let mut directory = store.root().to_path_buf();
    for component in ["plugins", "packages", id.as_str(), &version.to_string()] {
        directory.push(component);
        let metadata = fs::symlink_metadata(&directory).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                BootPackageError::Missing
            } else {
                BootPackageError::Unavailable
            }
        })?;
        if !metadata.is_dir() || !safe_metadata(&metadata) {
            return Err(BootPackageError::Corrupt);
        }
    }
    let manifest = validator.validate(&directory, id, version, source)?;
    Ok((directory, manifest))
}

/// 固定文案可持久化；插件提供的错误字符串不会成为信任状态。
fn failure_text(reason: BootPackageError) -> &'static str {
    match reason {
        BootPackageError::Missing => "插件版本目录不存在",
        BootPackageError::Corrupt => "插件版本损坏",
        BootPackageError::Incompatible => "插件与当前 API 不兼容",
        BootPackageError::Unavailable => "插件版本暂时无法读取",
    }
}

/// 启动调用方必须已取得单实例身份；未识别的条目保留并报告，避免误删外部文件。
fn clean_staging(root: &Path) -> std::io::Result<()> {
    check_cleanup_root(root)?;
    let directory = root.join("plugins").join("staging");
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || !safe_metadata(&metadata) {
        return Err(std::io::Error::other("invalid staging directory"));
    }
    let mut failed = false;
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_dir()
            || !safe_metadata(&metadata)
            || entry
                .file_name()
                .to_str()
                .and_then(|name| uuid::Uuid::parse_str(name).ok())
                .is_none()
        {
            failed = true;
            continue;
        }
        if !safe_tree(&entry.path())? || fs::remove_dir_all(entry.path()).is_err() {
            failed = true;
        }
    }
    if failed {
        Err(std::io::Error::other("staging cleanup incomplete"))
    } else {
        Ok(())
    }
}

/// 按最新已提交状态清理旧版本和卸载残留；未识别目录及待处理插件全部保留。
fn clean_unreferenced_packages(root: &Path, state: &PluginStoreState) -> std::io::Result<()> {
    check_cleanup_root(root)?;
    let parent = root.join("plugins").join("packages");
    match fs::symlink_metadata(&parent) {
        Ok(metadata) if metadata.is_dir() && safe_metadata(&metadata) => {}
        Ok(_) => return Err(std::io::Error::other("invalid package directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    // 启动必须已持有单实例身份，安装服务尚未启动；锁保护清理期间的状态引用。
    let pending = state
        .pending
        .iter()
        .map(PendingPluginChange::plugin_id)
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut failed = false;
    for entry in fs::read_dir(&parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|value| PluginId::parse(value).ok()) else {
            failed = true;
            continue;
        };
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_dir() || !safe_metadata(&metadata) {
            failed = true;
            continue;
        }
        if pending.contains(&id) {
            continue;
        }
        let installed = state.plugins.get(&id);
        for version_entry in fs::read_dir(entry.path())? {
            let version_entry = version_entry?;
            let name = version_entry.file_name();
            let Some(version) = name
                .to_str()
                .and_then(|value| PluginVersion::parse(value).ok())
            else {
                failed = true;
                continue;
            };
            let metadata = fs::symlink_metadata(version_entry.path())?;
            if !metadata.is_dir() || !safe_metadata(&metadata) {
                failed = true;
                continue;
            }
            // Windows 目录大小写不敏感；保守保留名称折叠后仍被引用的版本。
            let version_name = version.to_string();
            if installed.is_some_and(|plugin| {
                plugin
                    .active
                    .to_string()
                    .eq_ignore_ascii_case(&version_name)
                    || plugin.previous.as_ref().is_some_and(|previous| {
                        previous.to_string().eq_ignore_ascii_case(&version_name)
                    })
            }) {
                continue;
            }
            if !safe_tree(&version_entry.path())?
                || fs::remove_dir_all(version_entry.path()).is_err()
            {
                failed = true;
            }
        }
        if installed.is_none() && fs::remove_dir(entry.path()).is_err() {
            failed = true;
        }
    }
    if failed {
        Err(std::io::Error::other("package cleanup incomplete"))
    } else {
        Ok(())
    }
}

/// 先检查插件根本身，防止 packages 或 staging 的父目录被替换为重解析点。
fn check_cleanup_root(root: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(root.join("plugins"))?;
    if !metadata.is_dir() || !safe_metadata(&metadata) {
        return Err(std::io::Error::other("invalid plugin cleanup root"));
    }
    Ok(())
}

/// 删除前遍历真实目录并限制条目数量；链接、重解析点及未知文件类型一律保留。
fn safe_tree(root: &Path) -> std::io::Result<bool> {
    let mut pending = vec![root.to_path_buf()];
    let mut count = 0usize;
    while let Some(directory) = pending.pop() {
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || !safe_metadata(&metadata) {
            return Ok(false);
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            count += 1;
            if count > kerminal_plugin_protocol::MAX_PACKAGE_ENTRIES {
                return Ok(false);
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if !safe_metadata(&metadata) {
                return Ok(false);
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if !metadata.is_file() {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Windows junction 与其他重解析点都不允许参与启动和清理路径。
fn safe_metadata(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        !metadata.file_type().is_symlink() && metadata.file_attributes() & 0x400 == 0
    }
    #[cfg(not(windows))]
    {
        !metadata.file_type().is_symlink()
    }
}
