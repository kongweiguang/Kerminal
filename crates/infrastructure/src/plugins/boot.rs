//! 启动时冻结插件选择，并把损坏隔离在单个插件范围内。
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use kerminal_application::{
    external_contributions::PreparedPlugin,
    plugins::{CORE_PLUGIN_ID, is_builtin_unit, is_reserved_plugin_id},
};
use kerminal_domain::plugins::{
    InstalledPlugin, PendingPluginChange, PluginBootMarker, PluginBootPhase, PluginFingerprint,
    PluginId, PluginInstallStatus, PluginSource, PluginStoreState, PluginUnsupportedReason,
    PluginVersion,
};
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};

use super::{
    layout::{self, LayoutAction, LayoutError, LayoutPhase, PluginLayoutJournal},
    store::{PluginStore, PluginStoreError, PluginStoreSnapshot, layout_error},
};

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
            PluginRuntime::None { .. } => Platform::Any,
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
    /// 旧 script 清单仍保留目录以便卸载，但当前协议不再解析或执行。
    Unsupported,
    /// 手工放入的插件尚未确认；普通 Enable 不能跳过指纹确认直接把内容标记为信任。
    Unconfirmed,
    /// 单版本布局取消了人工回退，服务端必须显式拒绝旧 pending。
    RollbackRemoved,
    /// 核心单元承载基础装配，停用后没有可替代的启动根。
    ImmutableCore,
    /// 外部插件目录或操作占用了内置单元身份，不能把两者混为同一能力。
    ReservedIdentity,
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
            // 旧状态可能保留占用内置身份的外部目录；离线工具同样不能读取它，
            // 否则会把内置单元与外部内容重复暴露给同一装配入口。
            if is_reserved_plugin_id(id.as_str()) {
                rejected.insert(id.clone(), BootPackageError::ReservedIdentity);
                continue;
            }
            match read_record_package(self, validator, id, installed) {
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

    /// 目录改名、journal 写回与状态 CAS 在同一把存储锁内完成；启动失败时不留下
    /// 半换过的目录，单个插件损坏也不会阻止其他插件启动。
    pub fn begin_startup<V: BootPackageValidator>(
        &self,
        validator: &V,
        requested_safe_mode: bool,
    ) -> Result<BootSnapshot<V::Manifest>, PluginStoreError> {
        self.with_write_lock(|current| {
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

            // 取得写锁后先回收未认领的解包材料，失败只报告清理警告，不阻止启动。
            let mut cleanup_failed = !layout::cleanup_incoming(self.root()).unwrap_or(false);
            // 再消化上次未完成的目录事务；证据不足时保留现场并阻断启动事务。
            cleanup_failed |= layout::recover_journals(self.root(), &next).map_err(layout_error)?;

            let mut rejected = BTreeMap::new();
            let mut blocked = BTreeSet::new();
            let mut retained = Vec::new();
            let mut journals = Vec::new();
            for change in std::mem::take(&mut next.pending) {
                let id = change.plugin_id().clone();
                if blocked.contains(&id) {
                    retained.push(change);
                    continue;
                }
                match apply_change(self, validator, &mut next, &change, &mut journals) {
                    Ok(()) => {
                        next.failures.remove(&id);
                        next.pending_failures.remove(&id);
                    }
                    Err(ApplyFailure::Plugin(reason)) => {
                        blocked.insert(id.clone());
                        next.failures
                            .insert(id.clone(), failure_text(reason).to_owned());
                        // 只有启动应用 pending 失败才把 failures 与待处理意图绑定；
                        // 撤销入口据此拒绝把旧失败错配给后来排队的新操作。
                        next.pending_failures.insert(id.clone());
                        rejected.insert(id, reason);
                        retained.push(change);
                    }
                    Err(ApplyFailure::Fatal(error)) => {
                        // 致命错误发生在已知的目录阶段；尽量还原本次已完成的移动。
                        if rollback_journals(self.root(), &mut journals).is_err() {
                            return Err(PluginStoreError::LayoutRecoveryRequired {
                                journal_path: layout::plugin_root(self.root())
                                    .join(layout::MIGRATION_DIRECTORY),
                            });
                        }
                        return Err(error);
                    }
                }
            }
            next.pending = retained;

            // drop-in 发现必须发生在读取已安装记录之前，否则首次放入的目录会被误判缺失。
            discover_dropins(self, validator, &mut next, &mut rejected)?;
            let plugins = refresh_plugins(self, validator, &mut next, &blocked, &mut rejected);
            // 发现阶段失败但不产生记录的目录（例如清单损坏）会在目录删除后留下
            // 无归属文案；启动复验完成后按“无记录、无 pending、本次未发现”清理。
            prune_orphan_failures(&mut next, &rejected);

            let store = match self.compare_and_swap_locked(current, next) {
                Ok(store) => store,
                Err(error) => {
                    // 状态可能已提交（结果未知或只是清理失败）时目录可能已经生效，
                    // 不能回滚；保留 journal 与恢复材料等下一次启动继续判定。
                    if !state_may_be_committed(&error)
                        && rollback_journals(self.root(), &mut journals).is_err()
                    {
                        return Err(PluginStoreError::LayoutRecoveryRequired {
                            journal_path: layout::plugin_root(self.root())
                                .join(layout::MIGRATION_DIRECTORY),
                        });
                    }
                    return Err(error);
                }
            };
            cleanup_failed |= !finalize_journals(self.root(), &mut journals);
            Ok(BootSnapshot {
                boot_id,
                safe_mode,
                store,
                plugins,
                rejected,
                cleanup_failed,
            })
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

/// 应用失败分为“本插件可记录、保留 pending”和“整个启动事务必须中止”两类；
/// 目录结果未知或恢复证据不足绝不能降级成普通插件失败后继续写状态。
enum ApplyFailure {
    Plugin(BootPackageError),
    Fatal(PluginStoreError),
}

impl From<BootPackageError> for ApplyFailure {
    /// 包内容问题只影响当前插件。
    fn from(error: BootPackageError) -> Self {
        Self::Plugin(error)
    }
}

impl From<LayoutError> for ApplyFailure {
    /// 布局未知结果与恢复证据不足需要中止整个事务，不能冒充插件损坏。
    fn from(error: LayoutError) -> Self {
        match error {
            LayoutError::OutcomeUnknown { .. } | LayoutError::RecoveryRequired { .. } => {
                Self::Fatal(layout_error(error))
            }
            LayoutError::Io(source) => Self::Fatal(PluginStoreError::Io(source)),
            other => Self::Fatal(layout_error(other)),
        }
    }
}

/// 内置单元的开关以缺省启用表示：启用即删除覆盖项，停用写入 false。
/// 使用缺省值让旧状态无需一次写入补齐全部内置开关，也让新增单元自动保持启用。
fn apply_builtin_change(
    state: &mut PluginStoreState,
    change: &PendingPluginChange,
) -> Result<(), ApplyFailure> {
    let id = change.plugin_id();
    match change {
        PendingPluginChange::Enable { .. } => {
            state.builtins.remove(id);
        }
        PendingPluginChange::Disable { .. } => {
            if id.as_str() == CORE_PLUGIN_ID {
                return Err(BootPackageError::ImmutableCore.into());
            }
            state.builtins.insert(id.clone(), false);
        }
        PendingPluginChange::Install { .. }
        | PendingPluginChange::Update { .. }
        | PendingPluginChange::Uninstall { .. } => {
            return Err(BootPackageError::ReservedIdentity.into());
        }
        PendingPluginChange::Rollback { .. } => {
            return Err(BootPackageError::RollbackRemoved.into());
        }
    }
    Ok(())
}

/// 同一插件较早的操作失败时保留后续操作，避免顺序被错误地重排。
/// 成功返回时目录已经位于活动位置、状态投影也已更新；journal 留给 CAS 后收尾。
fn apply_change<V: BootPackageValidator>(
    store: &PluginStore,
    validator: &V,
    state: &mut PluginStoreState,
    change: &PendingPluginChange,
    journals: &mut Vec<PluginLayoutJournal>,
) -> Result<(), ApplyFailure> {
    // 内置单元的启停只改开关投影，不读也不能改写 `state.plugins` 或插件目录；
    // 这样旧外部目录即使占用了内置身份，也不会被这条 pending 重新解释。
    if is_builtin_unit(change.plugin_id().as_str()) {
        return apply_builtin_change(state, change);
    }
    let root = store.root();
    match change {
        PendingPluginChange::Install {
            id,
            to,
            staged,
            fingerprint,
            source,
            installed_at,
        } => {
            if state.plugins.contains_key(id) {
                return Err(BootPackageError::Corrupt.into());
            }
            let after = fingerprint.clone().ok_or(BootPackageError::Corrupt)?;
            let journal =
                apply_install_directory(store, validator, id, to, staged, after.clone(), *source)?;
            journals.push(journal);
            state.plugins.insert(
                id.clone(),
                InstalledPlugin {
                    active: to.clone(),
                    previous: None,
                    enabled: true,
                    source: *source,
                    installed_at: installed_at.clone(),
                    fingerprint: Some(after),
                    status: PluginInstallStatus::Ready,
                    confirmed: true,
                },
            );
        }
        PendingPluginChange::Update {
            id,
            to,
            staged,
            fingerprint,
        } => {
            let installed = state
                .plugins
                .get(id)
                .cloned()
                .ok_or(BootPackageError::Missing)?;
            let after = fingerprint.clone().ok_or(BootPackageError::Corrupt)?;
            if installed.status == PluginInstallStatus::Missing {
                // 目录缺失没有可比较的 before：按 Install 形态落盘重建记录，
                // 成功后只恢复版本与摘要，保留用户原有启停、来源和安装时间。
                let journal = apply_install_directory(
                    store,
                    validator,
                    id,
                    to,
                    staged,
                    after.clone(),
                    installed.source,
                )?;
                journals.push(journal);
                let installed = state.plugins.get_mut(id).expect("checked above");
                installed.active = to.clone();
                installed.previous = None;
                installed.fingerprint = Some(after);
                installed.status = PluginInstallStatus::Ready;
                return Ok(());
            }
            if installed.active == *to && installed.fingerprint.as_ref() == Some(&after) {
                // 目录和记录都已生效，只差状态收尾；重复应用必须幂等。
                return Ok(());
            }
            let before = installed
                .fingerprint
                .clone()
                .ok_or(BootPackageError::Corrupt)?;
            // 先读取已有 journal：恢复可能已推进到 NewInstalled，staging 已被移走。
            let mut created = false;
            let mut journal = match layout::read_operation_journal(root, staged)? {
                Some((_, journal)) => {
                    if journal.id != *id || journal.action != LayoutAction::Update {
                        return Err(ApplyFailure::Fatal(
                            PluginStoreError::LayoutRecoveryRequired {
                                journal_path: layout::journal_path(root, staged)?,
                            },
                        ));
                    }
                    journal
                }
                None => {
                    created = true;
                    PluginLayoutJournal::update(
                        root,
                        id,
                        staged,
                        before.clone(),
                        after.clone(),
                        after.clone(),
                    )?
                }
            };
            let final_path = layout::package_directory(root, id);
            let backup_path = layout::backup_directory(root, id, staged)?;
            if journal.phase == LayoutPhase::Prepared {
                if !layout::exists(&final_path)? && !layout::exists(&backup_path)? {
                    // 事务没有任何目录证据：删除待用 journal 并按可记录失败处理，
                    // 避免每次启动都在 execute_moves 里变成致命 Missing。
                    rollback_journal(root, &mut journal)
                        .map_err(|error| ApplyFailure::Fatal(layout_error(error)))?;
                    return Err(BootPackageError::Missing.into());
                }
                let staging_path = layout::staging_directory(root, id, staged)?;
                let staging_fingerprint = match layout::fingerprint_tree(&staging_path) {
                    Ok(fingerprint) => fingerprint,
                    Err(LayoutError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Err(BootPackageError::Missing.into());
                    }
                    Err(LayoutError::Io(error)) => {
                        return Err(ApplyFailure::Fatal(PluginStoreError::Io(error)));
                    }
                    Err(_) => return Err(BootPackageError::Corrupt.into()),
                };
                if staging_fingerprint != after {
                    return Err(BootPackageError::Corrupt.into());
                }
                // 活动目录被手工改动时不换位：先记录损坏，保留用户现场供卸载或重装。
                if layout::exists(&final_path)? {
                    match layout::fingerprint_tree(&final_path) {
                        Ok(actual) if actual != before => {
                            return Err(BootPackageError::Corrupt.into());
                        }
                        Ok(_) => {}
                        Err(_) => return Err(BootPackageError::Corrupt.into()),
                    }
                }
            }
            if created {
                // 校验通过后才落 journal，避免失败 pending 被无进展的事务挡住撤销。
                layout::write_journal(root, &journal)?;
            }
            if matches!(journal.phase, LayoutPhase::Prepared | LayoutPhase::OldMoved) {
                execute_moves(root, &mut journal)?;
            }
            if let Err(failure) =
                validate_installed_directory(store, validator, id, to, installed.source)
            {
                rollback_journal(root, &mut journal)
                    .map_err(|error| ApplyFailure::Fatal(layout_error(error)))?;
                return Err(failure);
            }
            if let Some(evidence) = &journal.after
                && !layout::matches_fingerprint(
                    &layout::package_directory(root, id),
                    &evidence.fingerprint,
                )?
            {
                rollback_journal(root, &mut journal)
                    .map_err(|error| ApplyFailure::Fatal(layout_error(error)))?;
                return Err(BootPackageError::Corrupt.into());
            }
            journals.push(journal.clone());
            let installed = state.plugins.get_mut(id).expect("checked above");
            installed.active = to.clone();
            installed.previous = None;
            installed.fingerprint = Some(after);
            installed.status = PluginInstallStatus::Ready;
        }
        PendingPluginChange::Rollback { .. } => {
            return Err(BootPackageError::RollbackRemoved.into());
        }
        PendingPluginChange::Enable { id } => {
            let installed = state.plugins.get_mut(id).ok_or(BootPackageError::Missing)?;
            if installed.source == PluginSource::DropIn && !installed.confirmed {
                // 确认信任必须由 confirm 路径核对指纹后写入；普通 Enable 不能在这里补确认，
                // 否则旧界面或第三方调用可跳过指纹校验直接启用手工放入的内容。
                // 返回可记录的插件失败而不是装配非法状态，避免整个启动 CAS 被拒绝。
                return Err(BootPackageError::Unconfirmed.into());
            }
            installed.enabled = true;
        }
        PendingPluginChange::Disable { id } => {
            // 未确认的 drop-in 本来就未启用，Disable 保持幂等；卸载仍可正常进行。
            let installed = state.plugins.get_mut(id).ok_or(BootPackageError::Missing)?;
            installed.enabled = false;
        }
        PendingPluginChange::Uninstall { id, operation_id } => {
            let installed = state
                .plugins
                .get(id)
                .cloned()
                .ok_or(BootPackageError::Missing)?;
            let before = match installed.fingerprint.clone() {
                Some(fingerprint) => fingerprint,
                None => {
                    // 迁移留下的 Missing 记录没有目录可证明；确认目录仍不存在后
                    // 只提交状态移除，不能为不存在的目录伪造 journal 证据。
                    if layout::exists(&layout::package_directory(root, id))? {
                        return Err(BootPackageError::Corrupt.into());
                    }
                    state.plugins.remove(id);
                    return Ok(());
                }
            };
            let existing = layout::read_operation_journal(root, operation_id)?;
            let created = existing.is_none();
            let mut journal = match existing {
                Some((_, journal)) => {
                    if journal.id != *id || journal.action != LayoutAction::Uninstall {
                        return Err(ApplyFailure::Fatal(
                            PluginStoreError::LayoutRecoveryRequired {
                                journal_path: layout::journal_path(root, operation_id)?,
                            },
                        ));
                    }
                    journal
                }
                None => {
                    // 新建事务先确认活动路径仍是安全目录；被替换成文件或链接时
                    // 只记可撤销失败，不能先落 journal 再在 move 里变成致命错误。
                    let final_path = layout::package_directory(root, id);
                    match fs::symlink_metadata(&final_path) {
                        Ok(metadata) if metadata.is_dir() && safe_metadata(&metadata) => {}
                        Ok(_) => return Err(ApplyFailure::Plugin(BootPackageError::Corrupt)),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(ApplyFailure::Fatal(PluginStoreError::Io(error)));
                        }
                    }
                    PluginLayoutJournal::uninstall(root, id, operation_id, before.clone())?
                }
            };
            if created {
                layout::write_journal(root, &journal)?;
            }
            execute_moves(root, &mut journal)?;
            journals.push(journal.clone());
            state.plugins.remove(id);
        }
    }
    Ok(())
}

/// 安装形态的目录事务：把 staging 移到 `plugins/<id>`，并复用上次崩溃留下的 journal。
///
/// Update 修复缺失记录时也走这里：没有旧目录可作 before 证据，只能按 Install 记录
/// after，避免伪造迁移阶段无法计算的旧指纹。返回已推进的 journal，由调用方随 CAS 收尾。
fn apply_install_directory<V: BootPackageValidator>(
    store: &PluginStore,
    validator: &V,
    id: &PluginId,
    to: &PluginVersion,
    staged: &str,
    after: PluginFingerprint,
    source: PluginSource,
) -> Result<PluginLayoutJournal, ApplyFailure> {
    let root = store.root();
    let final_path = layout::package_directory(root, id);
    // 恢复可能已把 staging 移入活动位置；先读 journal 再决定校验哪份证据。
    let mut created = false;
    let mut journal = match layout::read_operation_journal(root, staged)? {
        Some((_, journal)) => {
            if journal.id != *id || journal.action != LayoutAction::Install {
                return Err(ApplyFailure::Fatal(
                    PluginStoreError::LayoutRecoveryRequired {
                        journal_path: layout::journal_path(root, staged)?,
                    },
                ));
            }
            journal
        }
        None => {
            created = true;
            PluginLayoutJournal::install(root, id, staged, after.clone(), after.clone())?
        }
    };
    if matches!(journal.phase, LayoutPhase::Prepared | LayoutPhase::OldMoved) {
        let final_exists = layout::exists(&final_path)?;
        if final_exists {
            // 活动位置可能来自上次改名或手工占用；只有内容等于 after 才能前滚，
            // 否则先记插件失败，避免 execute_moves 的冲突升级成启动致命错误。
            match layout::fingerprint_tree(&final_path) {
                Ok(actual) if actual == after => {}
                _ => return Err(ApplyFailure::Plugin(BootPackageError::Corrupt)),
            }
        }
        let staging_path = layout::staging_directory(root, id, staged)?;
        let staging_fingerprint = match layout::fingerprint_tree(&staging_path) {
            Ok(fingerprint) => Some(fingerprint),
            Err(LayoutError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                // staging 已改名到活动位置时，execute_moves 会按 after 复核前滚；
                // 两份证据都缺失才说明安装材料丢失。
                if !final_exists {
                    return Err(BootPackageError::Missing.into());
                }
                None
            }
            Err(LayoutError::Io(error)) => {
                return Err(ApplyFailure::Fatal(PluginStoreError::Io(error)));
            }
            Err(_) => return Err(ApplyFailure::Plugin(BootPackageError::Corrupt)),
        };
        if staging_fingerprint
            .as_ref()
            .is_some_and(|value| value != &after)
        {
            // 指纹不匹配时不做任何移动，保留暂存目录供人工检查。
            return Err(ApplyFailure::Plugin(BootPackageError::Corrupt));
        }
    }
    if created {
        // 校验通过后才落 journal，失败 pending 不会留下无进展事务阻挡撤销。
        layout::write_journal(root, &journal)?;
    }
    if matches!(journal.phase, LayoutPhase::Prepared | LayoutPhase::OldMoved) {
        execute_moves(root, &mut journal)?;
    }
    if let Err(failure) = validate_installed_directory(store, validator, id, to, source) {
        // 只还原当前操作；之前已成功的插件目录保持新状态。
        rollback_journal(root, &mut journal)
            .map_err(|error| ApplyFailure::Fatal(layout_error(error)))?;
        return Err(failure);
    }
    if let Some(evidence) = &journal.after
        && !layout::matches_fingerprint(&final_path, &evidence.fingerprint)?
    {
        rollback_journal(root, &mut journal)
            .map_err(|error| ApplyFailure::Fatal(layout_error(error)))?;
        return Err(BootPackageError::Corrupt.into());
    }
    Ok(journal)
}

/// 推进到 NewInstalled；每一步先写 journal，失败结果未知时无法继续。
fn execute_moves(root: &Path, journal: &mut PluginLayoutJournal) -> Result<(), LayoutError> {
    let final_path = layout::package_directory(root, &journal.id);
    match journal.phase {
        LayoutPhase::Prepared => match journal.action {
            LayoutAction::Install => {
                if layout::exists(&final_path)? {
                    let after = journal
                        .after
                        .as_ref()
                        .ok_or(LayoutError::Conflict("插件安装缺少目标指纹"))?;
                    if layout::fingerprint_tree(&final_path)? != after.fingerprint {
                        return Err(LayoutError::Conflict("插件安装目标已存在且内容不同"));
                    }
                } else {
                    let staging = layout::verify_evidence(
                        root,
                        journal
                            .staging
                            .as_ref()
                            .ok_or(LayoutError::Missing("插件安装缺少暂存目录"))?,
                    )?;
                    layout::move_directory(&staging, &final_path, &journal.operation_id)?;
                }
                advance_journal(root, journal, LayoutPhase::NewInstalled)
            }
            LayoutAction::Update => {
                let backup_path =
                    layout::backup_directory(root, &journal.id, &journal.operation_id)?;
                if layout::exists(&final_path)? {
                    let before = journal
                        .before
                        .as_ref()
                        .ok_or(LayoutError::Conflict("插件更新缺少旧目录指纹"))?;
                    let after = journal
                        .after
                        .as_ref()
                        .ok_or(LayoutError::Conflict("插件更新缺少目标指纹"))?;
                    let actual = layout::fingerprint_tree(&final_path)?;
                    if actual == after.fingerprint && layout::exists(&backup_path)? {
                        // 上次进程在改名后、写阶段前崩溃；目录证据已完整。
                        return advance_journal(root, journal, LayoutPhase::NewInstalled);
                    }
                    if actual != before.fingerprint {
                        return Err(LayoutError::Conflict("插件活动目录内容与记录不一致"));
                    }
                    layout::move_directory(&final_path, &backup_path, &journal.operation_id)?;
                } else if !layout::exists(&backup_path)? {
                    return Err(LayoutError::Missing("插件活动目录不存在"));
                }
                advance_journal(root, journal, LayoutPhase::OldMoved)?;
                let staging = layout::verify_evidence(
                    root,
                    journal
                        .staging
                        .as_ref()
                        .ok_or(LayoutError::Missing("插件更新缺少暂存目录"))?,
                )?;
                layout::move_directory(&staging, &final_path, &journal.operation_id)?;
                advance_journal(root, journal, LayoutPhase::NewInstalled)
            }
            LayoutAction::Uninstall => {
                let backup_path =
                    layout::backup_directory(root, &journal.id, &journal.operation_id)?;
                if layout::exists(&final_path)? {
                    layout::move_directory(&final_path, &backup_path, &journal.operation_id)?;
                }
                advance_journal(root, journal, LayoutPhase::NewInstalled)
            }
        },
        LayoutPhase::OldMoved => match journal.action {
            LayoutAction::Update => {
                let staging = layout::verify_evidence(
                    root,
                    journal
                        .staging
                        .as_ref()
                        .ok_or(LayoutError::Missing("插件更新缺少暂存目录"))?,
                )?;
                if layout::exists(&final_path)? {
                    return Err(LayoutError::Conflict("插件更新目标目录已存在"));
                }
                layout::move_directory(&staging, &final_path, &journal.operation_id)?;
                advance_journal(root, journal, LayoutPhase::NewInstalled)
            }
            LayoutAction::Uninstall => advance_journal(root, journal, LayoutPhase::NewInstalled),
            LayoutAction::Install => Err(LayoutError::Conflict("插件安装出现旧目录阶段")),
        },
        LayoutPhase::NewInstalled
        | LayoutPhase::StateCommitted
        | LayoutPhase::Committed
        | LayoutPhase::Cleaned => Ok(()),
    }
}

/// journal 阶段必须先持久化，再执行下一步文件系统动作。
fn advance_journal(
    root: &Path,
    journal: &mut PluginLayoutJournal,
    phase: LayoutPhase,
) -> Result<(), LayoutError> {
    journal.phase = phase;
    layout::write_journal(root, journal).map(|_| ())
}

/// 已知未提交或插件校验失败时还原本次事务，避免目录与状态错位。
fn rollback_journals(
    root: &Path,
    journals: &mut Vec<PluginLayoutJournal>,
) -> Result<(), PluginStoreError> {
    let mut failure = None;
    for mut journal in journals.drain(..) {
        if let Err(error) = rollback_journal(root, &mut journal) {
            failure = Some(layout_error(error));
        }
    }
    failure.map_or(Ok(()), Err)
}

/// 根据 journal 阶段逆序移动；任何改名结果未知都停止并保留 journal。
fn rollback_journal(root: &Path, journal: &mut PluginLayoutJournal) -> Result<(), LayoutError> {
    let final_path = layout::package_directory(root, &journal.id);
    let backup_path = layout::backup_directory(root, &journal.id, &journal.operation_id)?;
    let mut restored = false;
    match journal.phase {
        LayoutPhase::NewInstalled => {
            if matches!(journal.action, LayoutAction::Install | LayoutAction::Update)
                && layout::exists(&final_path)?
            {
                let staging = layout::resolve_relative(
                    root,
                    &journal
                        .staging
                        .as_ref()
                        .ok_or(LayoutError::Conflict("插件事务缺少暂存证据"))?
                        .relative,
                )?;
                if layout::exists(&staging)? {
                    return Err(LayoutError::Conflict("插件暂存位置已被占用，无法还原"));
                }
                layout::move_directory(&final_path, &staging, &journal.operation_id)?;
                restored = true;
            }
            if matches!(
                journal.action,
                LayoutAction::Update | LayoutAction::Uninstall
            ) && layout::exists(&backup_path)?
                && !layout::exists(&final_path)?
            {
                layout::move_directory(&backup_path, &final_path, &journal.operation_id)?;
                restored = true;
            }
        }
        LayoutPhase::OldMoved
            if matches!(
                journal.action,
                LayoutAction::Update | LayoutAction::Uninstall
            ) && layout::exists(&backup_path)?
                && !layout::exists(&final_path)? =>
        {
            layout::move_directory(&backup_path, &final_path, &journal.operation_id)?;
            restored = true;
        }
        LayoutPhase::Prepared
        | LayoutPhase::OldMoved
        | LayoutPhase::StateCommitted
        | LayoutPhase::Committed
        | LayoutPhase::Cleaned => {}
    }
    if restored {
        // 目录已还原但状态未提交；先持久化回 Prepared，后续删除失败时重试
        // 会按与新建事务相同的证据继续，而不是把还原后的目录误当新版本。
        advance_journal(root, journal, LayoutPhase::Prepared)?;
    }
    let directory = layout::migration_directory(root, &journal.operation_id)?;
    layout::remove_tree_verified(&directory)
}

/// 状态可能已经提交的错误类别；此时回滚目录会与权威记录错开。
fn state_may_be_committed(error: &PluginStoreError) -> bool {
    matches!(
        error,
        PluginStoreError::OutcomeUnknown { .. }
            | PluginStoreError::RecoveryCleanupFailed {
                state_committed: true,
                ..
            }
    )
}

/// CAS 成功后推进 journal 并清理被证明不再引用的 staging/backup。
fn finalize_journals(root: &Path, journals: &mut Vec<PluginLayoutJournal>) -> bool {
    let mut complete = true;
    for journal in journals.iter_mut() {
        if journal.phase == LayoutPhase::NewInstalled
            && advance_journal(root, journal, LayoutPhase::StateCommitted).is_err()
        {
            complete = false;
            continue;
        }
        if journal.phase == LayoutPhase::StateCommitted
            && advance_journal(root, journal, LayoutPhase::Committed).is_err()
        {
            complete = false;
            continue;
        }
        if journal.phase == LayoutPhase::Committed
            && !layout::finish_committed(root, journal).unwrap_or(false)
        {
            complete = false;
        }
    }
    journals.clear();
    complete
}

/// 已安装记录只对应 `plugins/<id>/`；读取前先按持久指纹核对内容。
fn read_record_package<V: BootPackageValidator>(
    store: &PluginStore,
    validator: &V,
    id: &PluginId,
    installed: &InstalledPlugin,
) -> Result<(PathBuf, V::Manifest), BootPackageError> {
    let directory = layout::package_directory(store.root(), id);
    match &installed.fingerprint {
        Some(expected) => match layout::fingerprint_tree(&directory) {
            Ok(actual) if &actual != expected => return Err(BootPackageError::Corrupt),
            Ok(_) => {}
            Err(LayoutError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(BootPackageError::Missing);
            }
            Err(_) => return Err(BootPackageError::Corrupt),
        },
        None => return Err(BootPackageError::Corrupt),
    }
    read_package_at(
        &directory,
        validator,
        id,
        &installed.active,
        installed.source,
    )
}

/// 目录身份只来自宿主推导；验证器仍须核对清单、身份、API 与平台。
fn read_package_at<V: BootPackageValidator>(
    directory: &Path,
    validator: &V,
    id: &PluginId,
    version: &PluginVersion,
    source: PluginSource,
) -> Result<(PathBuf, V::Manifest), BootPackageError> {
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(BootPackageError::Missing);
        }
        Err(_) => return Err(BootPackageError::Unavailable),
    };
    if !metadata.is_dir() || !safe_metadata(&metadata) {
        return Err(BootPackageError::Corrupt);
    }
    match validator.validate(directory, id, version, source) {
        Ok(manifest) => Ok((directory.to_path_buf(), manifest)),
        // script 清单在新协议里无法解析；受限探测只用于区分“旧运行时”与普通损坏。
        Err(BootPackageError::Corrupt)
            if probe_manifest(directory)
                .is_some_and(|manifest| manifest.runtime_kind == "script") =>
        {
            Err(BootPackageError::Unsupported)
        }
        Err(error) => Err(error),
    }
}

/// 目录换位后再次执行完整包校验；失败由调用方回滚本次 journal。
fn validate_installed_directory<V: BootPackageValidator>(
    store: &PluginStore,
    validator: &V,
    id: &PluginId,
    version: &PluginVersion,
    source: PluginSource,
) -> Result<(), ApplyFailure> {
    let directory = layout::package_directory(store.root(), id);
    read_package_at(&directory, validator, id, version, source).map(|_| ())?;
    Ok(())
}

/// 固定文案可持久化；插件提供的错误字符串不会成为信任状态。
fn failure_text(reason: BootPackageError) -> &'static str {
    match reason {
        BootPackageError::Missing => "插件目录不存在",
        BootPackageError::Corrupt => "插件目录损坏",
        BootPackageError::Incompatible => "插件与当前 API 不兼容",
        BootPackageError::Unavailable => "插件目录暂时无法读取",
        BootPackageError::Unsupported => "插件运行时已不再支持",
        BootPackageError::Unconfirmed => "手动放入的插件需要先确认启用",
        BootPackageError::RollbackRemoved => "插件回退功能已移除，请重新安装旧版本",
        BootPackageError::ImmutableCore => "核心插件不可停用",
        BootPackageError::ReservedIdentity => "插件身份与内置单元冲突",
    }
}

/// 扫描 `plugins/*` 中尚无状态记录的目录；确认前绝不加载、绝不自动启用。
fn discover_dropins<V: BootPackageValidator>(
    store: &PluginStore,
    validator: &V,
    state: &mut PluginStoreState,
    rejected: &mut BTreeMap<PluginId, BootPackageError>,
) -> Result<(), PluginStoreError> {
    let directory = layout::plugin_root(store.root());
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(PluginStoreError::Io(error)),
    };
    if !metadata.is_dir() || !safe_metadata(&metadata) {
        return Err(layout_error(LayoutError::Unsafe("插件根目录不安全")));
    }
    let pending_ids: BTreeSet<_> = state
        .pending
        .iter()
        .filter_map(|change| match change {
            PendingPluginChange::Install { id, .. } | PendingPluginChange::Update { id, .. } => {
                Some(id.clone())
            }
            _ => None,
        })
        .collect();
    let mut candidates = BTreeMap::new();
    for entry in fs::read_dir(&directory).map_err(PluginStoreError::Io)? {
        let entry = entry.map_err(PluginStoreError::Io)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if is_reserved_plugin_entry(name) {
            continue;
        }
        // 内置身份（含无法解析为 `publisher.name` 的提供者名）不是 drop-in；
        // 认领会把内置单元目录改成外部插件记录，且会为保留身份生成无法加载的记录。
        if is_reserved_plugin_id(name) {
            continue;
        }
        // 非目录或无法成为插件身份的条目属于外部材料，扫描不删除也不认领。
        let Ok(id) = PluginId::parse(name) else {
            continue;
        };
        // 仍被宿主 pending 引用的目录属于未完成事务，不能认领成用户 drop-in。
        if is_reserved_plugin_id(id.as_str())
            || state.plugins.contains_key(&id)
            || pending_ids.contains(&id)
        {
            continue;
        }
        candidates.insert(id, entry.path());
    }
    for (directory_id, path) in candidates {
        let fingerprint = match layout::fingerprint_tree(&path) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                record_plugin_failure(
                    state,
                    rejected,
                    &directory_id,
                    BootPackageError::Corrupt,
                    if matches!(error, LayoutError::Unsafe(_)) {
                        "插件目录包含链接或未知文件"
                    } else {
                        "插件目录无法安全读取"
                    },
                );
                continue;
            }
        };
        let Some(probed) = probe_manifest(&path) else {
            record_plugin_failure(
                state,
                rejected,
                &directory_id,
                BootPackageError::Corrupt,
                "插件清单损坏",
            );
            continue;
        };
        let Ok(manifest_id) = PluginId::parse(&probed.id) else {
            record_plugin_failure(
                state,
                rejected,
                &directory_id,
                BootPackageError::Corrupt,
                "插件清单身份无效",
            );
            continue;
        };
        let Ok(version) = PluginVersion::parse(&probed.version) else {
            record_plugin_failure(
                state,
                rejected,
                &directory_id,
                BootPackageError::Corrupt,
                "插件清单版本无效",
            );
            continue;
        };
        if manifest_id != directory_id {
            record_plugin_failure(
                state,
                rejected,
                &directory_id,
                BootPackageError::Corrupt,
                "插件目录名与插件 ID 不一致",
            );
            continue;
        }
        let installed_at = now_rfc3339();
        let status = if probed.runtime_kind == "script" {
            PluginInstallStatus::Unsupported {
                reason: PluginUnsupportedReason::ScriptRemoved,
            }
        } else {
            match validator.validate(&path, &directory_id, &version, PluginSource::DropIn) {
                Ok(_) => PluginInstallStatus::Ready,
                Err(BootPackageError::Unsupported) => PluginInstallStatus::Unsupported {
                    reason: PluginUnsupportedReason::ScriptRemoved,
                },
                Err(reason) => {
                    record_plugin_failure(
                        state,
                        rejected,
                        &directory_id,
                        reason,
                        failure_text(reason),
                    );
                    continue;
                }
            }
        };
        let unsupported = matches!(status, PluginInstallStatus::Unsupported { .. });
        state.plugins.insert(
            directory_id.clone(),
            InstalledPlugin {
                active: version,
                previous: None,
                enabled: false,
                source: PluginSource::DropIn,
                installed_at,
                fingerprint: Some(fingerprint),
                status,
                confirmed: false,
            },
        );
        if unsupported {
            // 记录保留是为了允许卸载；失败记录解释为什么目录存在却不能加载。
            state.failures.insert(
                directory_id.clone(),
                failure_text(BootPackageError::Unsupported).to_owned(),
            );
            rejected.insert(directory_id, BootPackageError::Unsupported);
        } else {
            state.failures.remove(&directory_id);
        }
    }
    Ok(())
}

/// 每次启动复验目录与指纹，并把可加载项冻结成启动快照。
fn refresh_plugins<V: BootPackageValidator>(
    store: &PluginStore,
    validator: &V,
    state: &mut PluginStoreState,
    blocked: &BTreeSet<PluginId>,
    rejected: &mut BTreeMap<PluginId, BootPackageError>,
) -> Vec<BootPlugin<V::Manifest>> {
    let ids: Vec<_> = state.plugins.keys().cloned().collect();
    let mut plugins = Vec::new();
    for id in ids {
        let mut installed = state.plugins.get(&id).cloned().expect("iterating own keys");
        // 占用内置身份的外部记录不能读取目录：身份冲突不是包损坏，也不该因为
        // 目录内容不同而变化；保留记录与目录，让用户仍可在插件列表中卸载。
        if is_reserved_plugin_id(id.as_str()) {
            installed.status = PluginInstallStatus::Unsupported {
                reason: PluginUnsupportedReason::ReservedIdentity,
            };
            installed.enabled = false;
            // 旧数据可能仍引用保留身份；留着 pending 会让每次启动重复失败，
            // 并因绑定存在而拒绝任何新操作。这里丢弃这些待处理意图，但保留
            // 记录和目录，让用户仍能看到固定原因并可撤销/卸载外部材料。
            let had_pending = state.pending.iter().any(|change| change.plugin_id() == &id);
            if had_pending {
                state.pending.retain(|change| change.plugin_id() != &id);
                state.failures.insert(
                    id.clone(),
                    failure_text(BootPackageError::ReservedIdentity).to_owned(),
                );
            } else {
                // 没有待处理意图时，旧失败文本已无对象，不能继续悬挂。
                state.failures.remove(&id);
            }
            // pending 已不存在或不再可撤销，绑定必须一并清除。
            state.pending_failures.remove(&id);
            rejected.insert(id.clone(), BootPackageError::ReservedIdentity);
            state.plugins.insert(id, installed);
            continue;
        }
        let directory = layout::package_directory(store.root(), &id);
        let actual = match fs::symlink_metadata(&directory) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                installed.status = PluginInstallStatus::Missing;
                state.failures.insert(
                    id.clone(),
                    failure_text(BootPackageError::Missing).to_owned(),
                );
                rejected.insert(id.clone(), BootPackageError::Missing);
                state.plugins.insert(id, installed);
                continue;
            }
            Ok(_) => match layout::fingerprint_tree(&directory) {
                Ok(fingerprint) => fingerprint,
                Err(_) => {
                    installed.status = PluginInstallStatus::Corrupt;
                    installed.enabled = false;
                    state.failures.insert(id.clone(), "插件目录损坏".into());
                    rejected.insert(id.clone(), BootPackageError::Corrupt);
                    state.plugins.insert(id, installed);
                    continue;
                }
            },
            Err(_) => {
                installed.status = PluginInstallStatus::Corrupt;
                installed.enabled = false;
                state
                    .failures
                    .insert(id.clone(), "插件目录暂时无法读取".into());
                rejected.insert(id.clone(), BootPackageError::Unavailable);
                state.plugins.insert(id, installed);
                continue;
            }
        };
        if installed.fingerprint.as_ref() != Some(&actual) {
            // 目录被手动替换或改写时禁止加载；保留记录供用户卸载或重新安装。
            installed.status = PluginInstallStatus::Corrupt;
            installed.enabled = false;
            state
                .failures
                .insert(id.clone(), "插件目录内容已变更".into());
            rejected.insert(id.clone(), BootPackageError::Corrupt);
            state.plugins.insert(id, installed);
            continue;
        }
        match read_package_at(
            &directory,
            validator,
            &id,
            &installed.active,
            installed.source,
        ) {
            Ok((directory, manifest)) => {
                installed.status = PluginInstallStatus::Ready;
                if !blocked.contains(&id) {
                    state.failures.remove(&id);
                    state.pending_failures.remove(&id);
                }
                let enabled = installed.enabled;
                let version = installed.active.clone();
                state.plugins.insert(id.clone(), installed);
                plugins.push(BootPlugin {
                    id,
                    version,
                    directory,
                    enabled,
                    manifest,
                });
            }
            Err(BootPackageError::Unsupported) => {
                installed.status = PluginInstallStatus::Unsupported {
                    reason: PluginUnsupportedReason::ScriptRemoved,
                };
                installed.enabled = false;
                state.failures.insert(
                    id.clone(),
                    failure_text(BootPackageError::Unsupported).to_owned(),
                );
                rejected.insert(id.clone(), BootPackageError::Unsupported);
                state.plugins.insert(id, installed);
            }
            Err(reason) => {
                installed.status = if reason == BootPackageError::Missing {
                    PluginInstallStatus::Missing
                } else {
                    PluginInstallStatus::Corrupt
                };
                // 包读取失败只说明包损坏，不构成可撤销的 pending 意图；
                // 若同一插件已有应用失败留下的绑定则保留，撤销仍针对那笔 pending。
                state
                    .failures
                    .insert(id.clone(), failure_text(reason).to_owned());
                rejected.insert(id.clone(), reason);
                state.plugins.insert(id, installed);
            }
        }
    }
    plugins
}

/// 只有没有任何归属的失败文案才是历史残留：有记录时由复验决定是否保留，
/// 有 pending 时仍可通过撤销入口处理，本次仍被发现的失败则继续展示。
fn prune_orphan_failures(
    state: &mut PluginStoreState,
    rejected: &BTreeMap<PluginId, BootPackageError>,
) {
    let orphaned: Vec<_> = state
        .failures
        .keys()
        .filter(|id| {
            !state.plugins.contains_key(*id)
                && !state.pending.iter().any(|change| change.plugin_id() == *id)
                && !state.pending_failures.contains(*id)
                && !rejected.contains_key(*id)
        })
        .cloned()
        .collect();
    for id in orphaned {
        state.failures.remove(&id);
    }
}

/// 失败记录必须绑定目录身份，但自定义原因只使用固定文案，不回显插件输入。
fn record_plugin_failure(
    state: &mut PluginStoreState,
    rejected: &mut BTreeMap<PluginId, BootPackageError>,
    id: &PluginId,
    reason: BootPackageError,
    text: &str,
) {
    state.failures.insert(id.clone(), text.to_owned());
    rejected.insert(id.clone(), reason);
}

/// 状态文件和恢复材料、共享目录与点目录都不是插件身份。
fn is_reserved_plugin_entry(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    name.starts_with('.')
        || name == layout::DATA_DIRECTORY
        || name == layout::LOGS_DIRECTORY
        || lower == layout::STATE_FILE
        || (lower.starts_with("state-recovery-") && lower.ends_with(".json"))
}

/// 受限只读探测只取 id、version 与运行类型，不解析视图、不执行、不写回。
struct ProbedManifest {
    id: String,
    version: String,
    runtime_kind: String,
}

fn probe_manifest(directory: &Path) -> Option<ProbedManifest> {
    let path = directory.join(kerminal_plugin_protocol::MANIFEST_FILE_NAME);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file()
        || !safe_metadata(&metadata)
        || metadata.len() == 0
        || metadata.len() > kerminal_plugin_protocol::MAX_MANIFEST_BYTES as u64
    {
        return None;
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(&path).ok()?;
    let opened = file.metadata().ok()?;
    if !opened.is_file() || !safe_metadata(&opened) || opened.len() != metadata.len() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(kerminal_plugin_protocol::MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > kerminal_plugin_protocol::MAX_MANIFEST_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some(ProbedManifest {
        id: value.get("id")?.as_str()?.to_owned(),
        version: value.get("version")?.as_str()?.to_owned(),
        runtime_kind: value.get("runtime")?.get("kind")?.as_str()?.to_owned(),
    })
}

/// 新记录时间使用 UTC RFC3339，保持与安装器一致且可人工阅读。
fn now_rfc3339() -> String {
    chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now()).to_rfc3339()
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
