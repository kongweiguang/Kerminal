//! 安装器只发布已校验的不可变版本，并通过状态仓储排队生效。
//! @author kongweiguang

mod archive;
mod download;
mod recovery;

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    extensions::{
        CatalogSelection, CatalogSource, ExtensionChange, ExtensionReceipt, InstallProgress,
        LocalPackageInspection, PluginPackageStore,
    },
    plugin_settings::PluginSettingsService,
};
use kerminal_domain::plugins::{
    PendingPluginChange, PluginId, PluginSource, PluginStoreState, PluginVersion,
};
use kerminal_plugin_protocol::{ManifestPolicy, Platform, ValidatedPackage};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};
use tokio_util::sync::CancellationToken;

use crate::storage::{AtomicCommitError, CommitMode, commit_staged_file};

use super::{
    boot::{BootPackageValidator, ProtocolBootValidator},
    catalog::CatalogClient,
    store::{PluginStore, PluginStoreError},
};

/// 所有入口共用目录信任、状态仓储和设置删除服务。
#[derive(Clone)]
pub struct PackageInstaller {
    store: PluginStore,
    catalog: Arc<CatalogClient>,
    settings: Arc<PluginSettingsService>,
    host_api: String,
    platform: Platform,
}

impl PackageInstaller {
    /// 平台和 API 由组合根固定，插件和界面不能替换宿主身份。
    pub fn new(
        store: PluginStore,
        catalog: Arc<CatalogClient>,
        settings: Arc<PluginSettingsService>,
        host_api: String,
        platform: Platform,
    ) -> Self {
        Self {
            store,
            catalog,
            settings,
            host_api,
            platform,
        }
    }

    /// 在同一可信快照中选择精确版本与平台；不能接受调用方拼接的 URL。
    async fn selected_asset(
        &self,
        selection: &CatalogSelection,
    ) -> AppResult<(kerminal_application::extensions::CatalogAsset, String)> {
        if selection.platform != Platform::Any && selection.platform != self.platform {
            return Err(AppError::invalid("插件资产平台不匹配"));
        }
        let snapshot = self
            .catalog
            .cached()
            .await?
            .ok_or_else(|| AppError::not_found("插件目录尚未加载"))?;
        let plugin = snapshot
            .catalog
            .plugins
            .iter()
            .find(|plugin| plugin.id == selection.id)
            .ok_or_else(|| AppError::not_found("插件目录条目不存在"))?;
        let version = plugin
            .versions
            .iter()
            .find(|version| version.version == selection.version)
            .ok_or_else(|| AppError::not_found("插件目录版本不存在"))?;
        let asset = version
            .assets
            .iter()
            .find(|asset| asset.platform == selection.platform)
            .ok_or_else(|| AppError::not_found("插件目录资产不存在"))?;
        Ok((asset.clone(), version.api.clone()))
    }

    /// 开始前检查剩余空间；未知挂载或整数溢出都不伪装成空间充足。
    fn stage(&self, bytes: u64) -> AppResult<Stage> {
        let root = self.store.root();
        let staging = checked_directory(root, &["plugins", "staging"])?;
        let canonical = fs::canonicalize(&staging)
            .map_err(|e| AppError::infrastructure_with_source("解析插件暂存路径失败", e))?;
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let disk = disks
            .list()
            .iter()
            .filter(|disk| path_on_mount(&canonical, disk.mount_point()))
            .max_by_key(|disk| disk.mount_point().components().count())
            .ok_or_else(|| AppError::infrastructure("无法确定插件暂存磁盘剩余空间"))?;
        let required = bytes
            .checked_mul(3)
            .ok_or_else(|| AppError::invalid("插件安装包大小无效"))?;
        if disk.available_space() < required {
            return Err(AppError::invalid("插件安装空间不足"));
        }
        let directory = staging.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&directory)
            .map_err(|e| AppError::infrastructure_with_source("创建插件暂存失败", e))?;
        Ok(Stage { directory })
    }

    /// 完整校验后才发布；已有损坏残留明确要求人工处理，取消只在提交边界之前有效。
    fn prepare_and_commit(
        &self,
        stage: &Stage,
        source: PluginSource,
        expected: Option<(&CatalogSelection, &str)>,
        progress: &tokio::sync::watch::Sender<InstallProgress>,
        cancellation: &CancellationToken,
    ) -> AppResult<ExtensionReceipt> {
        progress.send_replace(InstallProgress::Verifying);
        let extracted = stage.directory.join("package");
        fs::create_dir(&extracted)
            .map_err(|e| AppError::infrastructure_with_source("创建插件解压目录失败", e))?;
        archive::unpack(&stage.archive(), &extracted, cancellation)?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件安装已取消"));
        }
        let policy = if source == PluginSource::Official {
            ManifestPolicy::Official
        } else {
            ManifestPolicy::ThirdParty
        };
        let package = kerminal_plugin_protocol::validate_package(&extracted, policy)
            .map_err(|e| AppError::infrastructure_with_source("插件包校验失败", e))?;
        let id = PluginId::parse(&package.manifest.id)
            .map_err(|_| AppError::invalid("插件包身份无效"))?;
        let version = PluginVersion::parse(&package.manifest.version)
            .map_err(|_| AppError::invalid("插件包版本无效"))?;
        if let Some((selection, api)) = expected {
            if id != selection.id || version != selection.version || package.manifest.api != api {
                return Err(AppError::invalid("插件包与目录不匹配"));
            }
            package
                .validate_for_platform(selection.platform)
                .map_err(|_| AppError::invalid("插件包平台不匹配"))?;
        }
        let validator = ProtocolBootValidator {
            host_api: self.host_api.clone(),
            platform: self.platform,
        };
        validator
            .validate(&extracted, &id, &version, source)
            .map_err(|_| AppError::invalid("插件包与当前宿主不兼容"))?;
        let expected_state = self.store.load().map_err(store_error)?;
        let mut next = expected_state.state.clone();
        if next.pending.iter().any(|item| item.plugin_id() == &id) {
            return Err(AppError::conflict("插件已有待处理操作"));
        }
        let staged = format!("packages/{id}/{version}");
        let change = match next.plugins.get(&id) {
            Some(existing) if existing.source != source => {
                return Err(AppError::conflict("插件安装来源不匹配"));
            }
            Some(existing) if existing.active == version => {
                return Err(AppError::conflict("插件版本已经安装"));
            }
            Some(_) => PendingPluginChange::Update {
                id: id.clone(),
                to: version.clone(),
                staged,
            },
            None => PendingPluginChange::Install {
                id: id.clone(),
                to: version.clone(),
                staged,
                source,
                installed_at: chrono::DateTime::<chrono::Utc>::from(SystemTime::now()).to_rfc3339(),
            },
        };
        next.pending.push(change);
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件安装已取消"));
        }
        progress.send_replace(InstallProgress::Publishing);
        // 发布和 CAS 在同一个共享存储锁下执行，启动清理无法介入。
        let mut package_residue = false;
        let committed = self
            .store
            .publish_and_compare(&expected_state, next, || {
                publish(self.store.root(), &id, &version, &extracted, &package).inspect_err(
                    |error| {
                        package_residue = matches!(
                            error,
                            PluginStoreError::InvalidState | PluginStoreError::Conflict
                        );
                    },
                )
            })
            .map_err(|error| {
                if package_residue {
                    AppError::infrastructure_with_source(
                        "插件版本目录存在损坏或不安全残留，请人工处理后重装",
                        error,
                    )
                } else {
                    store_error(error)
                }
            })?;
        Ok(ExtensionReceipt {
            revision: committed.state.revision,
            restart_required: true,
            cleanup_warning: None,
        })
    }
}

/// 暂存目录以 UUID 命名，启动恢复可识别崩溃遗留并安全清理。
struct Stage {
    directory: PathBuf,
}
impl Stage {
    /// 显式清理失败必须反馈给调用方；崩溃时保留的目录由下次启动检查。
    fn cleanup(&self) -> AppResult<()> {
        match fs::remove_dir_all(&self.directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AppError::outcome_unknown_with_source(
                "插件暂存清理失败",
                error,
            )),
        }
    }
    /// 宿主固定归档名，不采纳插件路径。
    fn archive(&self) -> PathBuf {
        self.directory.join("pkg.kplugin")
    }
}
impl Drop for Stage {
    /// 临时目录只包含新建的普通文件；清理失败由下次启动恢复处理。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// Windows 暂存路径含规范化的扩展前缀，挂载点也须规范化；按分量比较避免目录名前缀误匹配。
fn path_on_mount(path: &Path, mount: &Path) -> bool {
    #[cfg(windows)]
    {
        fs::canonicalize(mount).is_ok_and(|mount| path.starts_with(mount))
    }
    #[cfg(not(windows))]
    {
        path.starts_with(mount)
    }
}

/// 自配置根逐级创建真实目录并拒绝重解析点，避免外部输入改变安装位置。
fn checked_directory(root: &Path, segments: &[&str]) -> AppResult<PathBuf> {
    let mut path = root.to_path_buf();
    for segment in segments {
        path.push(segment);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(AppError::infrastructure_with_source("创建插件目录失败", e)),
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|e| AppError::infrastructure_with_source("读取插件目录失败", e))?;
        if !metadata.is_dir() || !safe_metadata(&metadata) {
            return Err(AppError::invalid("插件目录路径不安全"));
        }
    }
    Ok(path)
}

/// Windows junction 和符号链接均不能进入发布路径。
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

/// 复制离线包并受取消控制；用户源文件从不被修改。
fn copy_local(source: &Path, target: &Path, cancellation: &CancellationToken) -> AppResult<()> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|e| AppError::infrastructure_with_source("读取本地插件包失败", e))?;
    if !metadata.is_file()
        || !safe_metadata(&metadata)
        || metadata.len() == 0
        || metadata.len() > 512 * 1024 * 1024
    {
        return Err(AppError::invalid("本地插件包无效"));
    }
    let mut input = File::open(source)
        .map_err(|e| AppError::infrastructure_with_source("读取本地插件包失败", e))?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)
        .map_err(|e| AppError::infrastructure_with_source("创建插件暂存失败", e))?;
    let mut copied = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件安装已取消"));
        }
        let read = input
            .read(&mut buffer)
            .map_err(|e| AppError::infrastructure_with_source("读取本地插件包失败", e))?;
        if read == 0 {
            break;
        }
        copied += read as u64;
        if copied > metadata.len() {
            return Err(AppError::conflict("本地插件包已更改"));
        }
        output
            .write_all(&buffer[..read])
            .map_err(|e| AppError::infrastructure_with_source("写入插件暂存失败", e))?;
    }
    if copied != metadata.len() {
        return Err(AppError::conflict("本地插件包已更改"));
    }
    output
        .sync_all()
        .map_err(|e| AppError::infrastructure_with_source("同步插件暂存失败", e))
}

/// 签名目录绑定的大小和摘要必须在解压前按完整下载文件核对。
fn verify_archive(
    path: &Path,
    size: u64,
    digest: &str,
    cancellation: &CancellationToken,
) -> AppResult<()> {
    let mut file = File::open(path)
        .map_err(|e| AppError::infrastructure_with_source("读取插件暂存失败", e))?;
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件安装已取消"));
        }
        let n = file
            .read(&mut buffer)
            .map_err(|e| AppError::infrastructure_with_source("读取插件暂存失败", e))?;
        if n == 0 {
            break;
        }
        count = count
            .checked_add(n as u64)
            .ok_or_else(|| AppError::invalid("插件包大小无效"))?;
        if count > size {
            return Err(AppError::invalid("插件包大小不匹配"));
        }
        hasher.update(&buffer[..n]);
    }
    if count != size || !format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(digest) {
        return Err(AppError::invalid("插件包摘要或大小不匹配"));
    }
    Ok(())
}

/// 文件和目录均已做包校验；同版本重用必须比较每个文件的实际字节。
fn publish(
    root: &Path,
    id: &PluginId,
    version: &PluginVersion,
    extracted: &Path,
    package: &ValidatedPackage,
) -> Result<(), PluginStoreError> {
    let parent = checked_directory(root, &["plugins", "packages", id.as_str()])
        .map_err(|_| PluginStoreError::InvalidState)?;
    let destination = parent.join(version.to_string());
    match fs::symlink_metadata(&destination) {
        Ok(metadata) => {
            if !metadata.is_dir() || !safe_metadata(&metadata) {
                return Err(PluginStoreError::InvalidState);
            }
            let existing =
                kerminal_plugin_protocol::validate_package(&destination, ManifestPolicy::Official)
                    .map_err(|_| PluginStoreError::InvalidState)?;
            if existing.files != package.files {
                return Err(PluginStoreError::Conflict);
            }
            for name in package.files.keys() {
                let left = file_digest(&extracted.join(name)).map_err(PluginStoreError::Io)?;
                let right = file_digest(&destination.join(name)).map_err(PluginStoreError::Io)?;
                if left != right {
                    return Err(PluginStoreError::Conflict);
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            sync_directories(extracted).map_err(PluginStoreError::Io)?;
            commit_staged_file(extracted, &destination, CommitMode::MoveNoReplace).map_err(
                |error| match error {
                    AtomicCommitError::NotCommitted(source) => PluginStoreError::Io(source),
                    AtomicCommitError::CommitUnknown(_) => PluginStoreError::OutcomeUnknown {
                        recovery_path: destination,
                        recovery_persisted: false,
                    },
                },
            )
        }
        Err(error) => Err(PluginStoreError::Io(error)),
    }
}

/// 固定缓冲区计算文件摘要，已有版本再大也不会要求同等大小的内存。
fn file_digest(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

/// 文件已逐个同步；Unix 再同步各级目录后才能发布整个版本目录。
fn sync_directories(root: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut directories = vec![root.to_path_buf()];
        let mut index = 0;
        while index < directories.len() {
            for entry in fs::read_dir(&directories[index])? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    directories.push(entry.path());
                }
            }
            index += 1;
        }
        for directory in directories.iter().rev() {
            File::open(directory)?.sync_all()?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = root;
    }
    Ok(())
}
/// 保留冲突和未知提交类别，其余底层错误留在诊断链中。
fn store_error(error: PluginStoreError) -> AppError {
    match error {
        PluginStoreError::Conflict => AppError::conflict("插件状态已更改"),
        error @ (PluginStoreError::OutcomeUnknown { .. }
        | PluginStoreError::RecoveryRequired { .. }
        | PluginStoreError::RecoveryPreparationUnknown { .. }
        | PluginStoreError::RecoveryCleanupFailed { .. }
        | PluginStoreError::RecoveryResolutionUnknown { .. }) => {
            AppError::outcome_unknown_with_source("插件状态需读回恢复", error)
        }
        error => AppError::infrastructure_with_source("插件存储操作失败", error),
    }
}

#[async_trait]
impl PluginPackageStore for PackageInstaller {
    /// 应用读取通过共享仓储执行，不创建第二份状态文件。
    async fn state(&self) -> AppResult<PluginStoreState> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            store
                .load()
                .map(|snapshot| snapshot.state)
                .map_err(store_error)
        })
        .await
        .map_err(|e| AppError::infrastructure_with_source("读取插件状态任务失败", e))?
    }

    /// 安装始终重新解析验签目录，大小检查先于下载。
    async fn install_catalog(
        &self,
        selection: CatalogSelection,
        progress: tokio::sync::watch::Sender<InstallProgress>,
        cancellation: CancellationToken,
    ) -> AppResult<ExtensionReceipt> {
        let (asset, api) = self.selected_asset(&selection).await?;
        let this = self.clone();
        let size = asset.size;
        let stage = tokio::task::spawn_blocking(move || this.stage(size))
            .await
            .map_err(|e| AppError::infrastructure_with_source("创建插件暂存任务失败", e))??;
        if let Err(error) = download::download(
            &asset,
            &stage.archive(),
            self.catalog.allows_loopback(),
            &progress,
            &cancellation,
        )
        .await
        {
            tokio::task::spawn_blocking(move || stage.cleanup())
                .await
                .map_err(|e| AppError::outcome_unknown_with_source("插件暂存清理任务异常", e))??;
            return Err(error);
        }
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let result = (|| {
                verify_archive(&stage.archive(), asset.size, &asset.sha256, &cancellation)?;
                this.prepare_and_commit(
                    &stage,
                    PluginSource::Official,
                    Some((&selection, &api)),
                    &progress,
                    &cancellation,
                )
            })();
            stage.cleanup()?;
            result
        })
        .await
        .map_err(|e| AppError::outcome_unknown_with_source("插件安装任务异常，请读回状态", e))?
    }

    /// 使用与正式安装相同的解包和宿主校验，摘要将后续确认绑定到这份实际内容。
    async fn inspect_file(
        &self,
        path: PathBuf,
        cancellation: CancellationToken,
    ) -> AppResult<LocalPackageInspection> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| AppError::infrastructure_with_source("读取本地插件包失败", e))?;
            if !metadata.is_file()
                || !safe_metadata(&metadata)
                || metadata.len() == 0
                || metadata.len() > 512 * 1024 * 1024
            {
                return Err(AppError::invalid("本地插件包无效"));
            }
            let stage = this.stage(metadata.len())?;
            let result = (|| {
                copy_local(&path, &stage.archive(), &cancellation)?;
                let digest = file_digest(&stage.archive())
                    .map_err(|e| AppError::infrastructure_with_source("读取插件暂存失败", e))?;
                let extracted = stage.directory.join("package");
                fs::create_dir(&extracted)
                    .map_err(|e| AppError::infrastructure_with_source("创建插件解压目录失败", e))?;
                archive::unpack(&stage.archive(), &extracted, &cancellation)?;
                let package = kerminal_plugin_protocol::validate_package(
                    &extracted,
                    ManifestPolicy::ThirdParty,
                )
                .map_err(|e| AppError::infrastructure_with_source("插件包校验失败", e))?;
                let id = PluginId::parse(&package.manifest.id)
                    .map_err(|_| AppError::invalid("插件包身份无效"))?;
                let version = PluginVersion::parse(&package.manifest.version)
                    .map_err(|_| AppError::invalid("插件包版本无效"))?;
                ProtocolBootValidator {
                    host_api: this.host_api.clone(),
                    platform: this.platform,
                }
                .validate(&extracted, &id, &version, PluginSource::Local)
                .map_err(|_| AppError::invalid("插件包与当前宿主不兼容"))?;
                Ok(LocalPackageInspection {
                    id,
                    version,
                    name: package.manifest.name.clone(),
                    permissions: package
                        .manifest
                        .effective_permissions()
                        .into_iter()
                        .map(|permission| permission.to_string())
                        .collect(),
                    size: metadata.len(),
                    sha256: digest.iter().map(|byte| format!("{byte:02x}")).collect(),
                })
            })();
            stage.cleanup()?;
            result
        })
        .await
        .map_err(|e| AppError::infrastructure_with_source("插件预检任务失败", e))?
    }

    /// 本地安装独立于目录来源，但仍执行同样的包与宿主兼容性验证。
    async fn install_file(
        &self,
        path: PathBuf,
        inspected: LocalPackageInspection,
        progress: tokio::sync::watch::Sender<InstallProgress>,
        cancellation: CancellationToken,
    ) -> AppResult<ExtensionReceipt> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| AppError::infrastructure_with_source("读取本地插件包失败", e))?;
            let stage = this.stage(metadata.len())?;
            let result = (|| {
                copy_local(&path, &stage.archive(), &cancellation)?;
                verify_archive(
                    &stage.archive(),
                    inspected.size,
                    &inspected.sha256,
                    &cancellation,
                )?;
                this.prepare_and_commit(&stage, PluginSource::Local, None, &progress, &cancellation)
            })();
            stage.cleanup()?;
            result
        })
        .await
        .map_err(|e| AppError::outcome_unknown_with_source("插件安装任务异常，请读回状态", e))?
    }

    /// 普通操作只排队；失败撤销走同一 CAS 门禁，不能借恢复入口绕过未知提交围栏。
    async fn change(&self, change: ExtensionChange) -> AppResult<ExtensionReceipt> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let expected = store.load().map_err(store_error)?;
            let change = match change {
                ExtensionChange::CancelFailedChange {
                    expected_revision,
                    pending,
                } => {
                    return recovery::cancel_failed_change(
                        &store,
                        &expected,
                        expected_revision,
                        &pending,
                    );
                }
                change => change,
            };
            let mut next = expected.state.clone();
            let id = match &change {
                ExtensionChange::Enable(id)
                | ExtensionChange::Disable(id)
                | ExtensionChange::Rollback(id)
                | ExtensionChange::Uninstall(id) => id,
                ExtensionChange::CancelFailedChange { .. } => {
                    return Err(AppError::invalid("失败变更取消入口无效"));
                }
            };
            if next.pending.iter().any(|item| item.plugin_id() == id) {
                return Err(AppError::conflict("插件已有待处理操作"));
            }
            let installed = next
                .plugins
                .get(id)
                .ok_or_else(|| AppError::not_found("插件未安装"))?;
            let pending = match change {
                ExtensionChange::Enable(id) if !installed.enabled => {
                    PendingPluginChange::Enable { id }
                }
                ExtensionChange::Disable(id) if installed.enabled => {
                    PendingPluginChange::Disable { id }
                }
                ExtensionChange::Rollback(id) => PendingPluginChange::Rollback {
                    id,
                    to: installed
                        .previous
                        .clone()
                        .ok_or_else(|| AppError::conflict("没有可回退版本"))?,
                },
                ExtensionChange::Uninstall(id) => PendingPluginChange::Uninstall { id },
                _ => return Err(AppError::conflict("插件已处于目标状态")),
            };
            next.pending.push(pending);
            let snapshot = store
                .compare_and_swap(&expected, next)
                .map_err(store_error)?;
            Ok(ExtensionReceipt {
                revision: snapshot.state.revision,
                restart_required: true,
                cleanup_warning: None,
            })
        })
        .await
        .map_err(|e| AppError::outcome_unknown_with_source("插件状态任务异常，请读回状态", e))?
    }

    /// 设置服务负责设置、数据和 Vault 的分步删除与未知结果报告。
    async fn delete_data(&self, id: PluginId) -> AppResult<()> {
        self.settings
            .delete_data(&id, CancellationToken::new())
            .await
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/plugin_install_flow.rs"]
mod tests;
